// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

//! DB-derived refresh loop for the indexer's OTLP metric counters. Every
//! `interval`, counts the tracked `raw_events` types and pushes the totals
//! into the metric caches the OTLP reader exports. Lives in the indexer (not
//! `dodex-infrastructure`) so the API service does not transitively depend on
//! opentelemetry / tonic.

use std::time::Duration;

use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_metrics::IndexerMetrics;
use tracing::debug;
use tracing::error;

/// `raw_events.event_type` whose total backs `orders_created_event_cnt`.
const ORDERS_CREATED_EVENT: &str = "OrderBook.OrderPlaced";
/// `raw_events.event_type` whose total backs `order_partially_filled_event_cnt`.
const ORDER_PARTIALLY_FILLED_EVENT: &str = "OrderBook.PartialFill";

/// Maps the per-type count rows to `(orders_created, orders_partially_filled)`.
/// Missing types default to 0; an (impossible) negative count clamps to 0;
/// untracked types are ignored.
fn resolve_counts(rows: &[(String, i64)]) -> (u64, u64) {
    let mut created = 0u64;
    let mut partially = 0u64;
    for (event_type, count) in rows {
        let count = (*count).max(0) as u64;
        match event_type.as_str() {
            ORDERS_CREATED_EVENT => created = count,
            ORDER_PARTIALLY_FILLED_EVENT => partially = count,
            _ => {}
        }
    }
    (created, partially)
}

/// Forever loop: refresh the metric caches every `interval`. Errors are logged
/// and the loop continues. Spawned only when OTLP metrics are enabled.
pub async fn run_refresh_loop(
    repo: IndexerRepository,
    interval: Duration,
    metrics: IndexerMetrics,
    cursor_stream: &'static str,
) {
    loop {
        refresh_once(&repo, &metrics, cursor_stream).await;
        tokio::time::sleep(interval).await;
    }
}

/// One refresh pass — the body of a `run_refresh_loop` iteration, extracted so
/// a test can drive a single pass (e.g. against a closed pool). Eight sections
/// query the DB and each has its own `Err` arm: log, bump the failure counter,
/// and skip that metric's `set_*` — freezing it at its last value. There is no
/// short-circuit between sections, so one dead DB costs exactly eight bumps
/// per pass. The in-memory sections (pool stats and the repo-owned counters)
/// have no `Err` arm and always store.
async fn refresh_once(repo: &IndexerRepository, metrics: &IndexerMetrics, cursor_stream: &str) {
    match repo.count_events_by_type(&[ORDERS_CREATED_EVENT, ORDER_PARTIALLY_FILLED_EVENT]).await {
        Ok(rows) => {
            let (created, partially) = resolve_counts(&rows);
            metrics.set_orders_created(created);
            metrics.set_orders_partially_filled(partially);
            debug!(created, partially, "metrics refresh");
        }
        Err(err) => {
            error!(?err, "metrics refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    match repo.count_pending_projection().await {
        Ok(n) => metrics.set_projection_backlog(n.max(0) as u64),
        Err(err) => {
            error!(?err, "projection backlog metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    match repo.projection_lag_seconds().await {
        Ok(s) => metrics.set_projection_lag_seconds(s.max(0) as u64),
        Err(err) => {
            error!(?err, "projection lag metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    match repo.cursor_age_seconds(cursor_stream).await {
        Ok(age) => metrics.set_capture_cursor_age_seconds(age.unwrap_or(0).max(0) as u64),
        Err(err) => {
            error!(?err, "cursor age metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    let (in_use, idle) = repo.pool_connection_stats();
    metrics.set_pool_connections(in_use, idle);
    metrics.set_projection_fallbacks(repo.projection_fallback_count());
    metrics.set_inference_orphans_dropped(repo.inference_orphans_dropped_count());
    metrics.set_decode_errors(repo.decode_errors_count());
    metrics.set_decode_ambiguous_collisions(repo.decode_ambiguous_collisions_count());
    match repo.inference_market_state_counts().await {
        Ok((discovering, visible, failing)) => metrics.set_inference_market_states(
            discovering.max(0) as u64,
            visible.max(0) as u64,
            failing.max(0) as u64,
        ),
        Err(err) => {
            error!(?err, "inference market state metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    match repo.inference_staleness_seconds().await {
        Ok((price_lag, sweep_lag)) => {
            metrics.set_inference_reference_price_lag_seconds(price_lag.max(0) as u64);
            metrics.set_inference_sweep_lag_seconds(sweep_lag.max(0) as u64);
        }
        Err(err) => {
            error!(?err, "inference staleness metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    match repo.inference_order_status_counts().await {
        Ok((open, filled, cancelled, expired)) => metrics.set_inference_order_counts(
            open.max(0) as u64,
            filled.max(0) as u64,
            cancelled.max(0) as u64,
            expired.max(0) as u64,
        ),
        Err(err) => {
            error!(?err, "inference order status metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
    metrics.set_inference_reconcile_failures(repo.inference_reconcile_failures_count());
    match repo.inference_wedged_books_count().await {
        Ok(n) => metrics.set_inference_wedged_books(n.max(0) as u64),
        Err(err) => {
            error!(?err, "inference wedged books metric refresh failed");
            metrics.inc_metrics_refresh_failures();
        }
    }
}

#[cfg(test)]
mod tests {
    use dodex_infrastructure::config::DatabaseSection;
    use dodex_infrastructure::config::METRIC_CRITICAL_EVENT_TYPES;
    use dodex_infrastructure::database;
    use dodex_infrastructure::indexer_repo::IndexerRepository;
    use dodex_metrics::IndexerMetrics;

    use super::refresh_once;
    use super::resolve_counts;
    use super::ORDERS_CREATED_EVENT;
    use super::ORDER_PARTIALLY_FILLED_EVENT;

    /// The config startup guard refuses to drop a metric-critical type, but it
    /// only knows the types listed in `METRIC_CRITICAL_EVENT_TYPES`. If a third
    /// metric-backed type is tracked here without being added there, the guard
    /// silently stops protecting it. Fail loudly instead of drifting.
    #[test]
    fn metric_critical_covers_tracked_types() {
        for tracked in [ORDERS_CREATED_EVENT, ORDER_PARTIALLY_FILLED_EVENT] {
            assert!(
                METRIC_CRITICAL_EVENT_TYPES.contains(&tracked),
                "metrics_refresh tracks {tracked:?} but config::METRIC_CRITICAL_EVENT_TYPES \
                 does not list it — the ignored_event_types guard would not protect it; \
                 add it to METRIC_CRITICAL_EVENT_TYPES"
            );
        }
    }

    #[test]
    fn maps_tracked_types() {
        let rows = vec![
            ("OrderBook.OrderPlaced".to_string(), 5),
            ("OrderBook.PartialFill".to_string(), 2),
            ("OrderBook.OrderFilled".to_string(), 99), // untracked — ignored
        ];
        assert_eq!(resolve_counts(&rows), (5, 2));
    }

    #[test]
    fn defaults_missing_to_zero() {
        let rows = vec![("OrderBook.OrderPlaced".to_string(), 4)];
        assert_eq!(resolve_counts(&rows), (4, 0));
    }

    #[test]
    fn clamps_negative() {
        let rows = vec![("OrderBook.PartialFill".to_string(), -1)];
        assert_eq!(resolve_counts(&rows), (0, 0));
    }

    #[test]
    fn ignores_untracked_types() {
        let rows = vec![("OrderBook.OrderFilled".to_string(), 7)];
        assert_eq!(resolve_counts(&rows), (0, 0));
    }

    // IX-MET-05: the only test of the refresh loop's failure arms. Connect to
    // the test DB (a pool must validate one live connection to build), close
    // the pool, and drive a single `refresh_once`: every DB query then fails
    // fast with PoolClosed. Sentinels sit on exactly the caches the eight
    // fallible sections write — including both events-by-type counter caches —
    // and NOT on the metrics updated without a query (pool-connections is
    // legitimately rewritten from the closed pool's live stats). Asserted:
    // (a) the failure counter grew by exactly 8 — the sections are independent
    // (each its own query + its own inc, no short-circuit), so fewer means a
    // skipped section or an early return and more means a double increment;
    // (b) every sentinel survived — freeze-on-error, not reset-to-zero; an
    // implementation zeroing gauges in the Err arm must fail this (zero
    // defaults could not tell the difference, hence non-zero sentinels).
    #[tokio::test]
    async fn refresh_once_on_a_closed_pool_freezes_gauges_and_counts_eight_failures() {
        let url = match std::env::var("TEST_DATABASE_URL") {
            Ok(v) if !v.is_empty() => v,
            _ => {
                eprintln!("skipping: TEST_DATABASE_URL not set");
                return;
            }
        };
        let cfg = DatabaseSection {
            url,
            max_connections: 2,
            min_connections: 0,
            connect_timeout_ms: 5_000,
        };
        let pool = database::build_pool(&cfg).await.expect("TEST_DATABASE_URL connect");
        let repo = IndexerRepository::new(pool.clone());
        let (_provider, metrics) = IndexerMetrics::new_in_memory_for_tests();

        metrics.set_orders_created(11);
        metrics.set_orders_partially_filled(12);
        metrics.set_projection_backlog(13);
        metrics.set_projection_lag_seconds(14);
        metrics.set_capture_cursor_age_seconds(15);
        metrics.set_inference_market_states(16, 17, 18);
        metrics.set_inference_reference_price_lag_seconds(19);
        metrics.set_inference_sweep_lag_seconds(20);
        metrics.set_inference_order_counts(21, 22, 23, 24);
        metrics.set_inference_wedged_books(25);

        pool.close().await;
        refresh_once(&repo, &metrics, "refresh_once_closed_pool_test_stream").await;

        assert_eq!(
            metrics.metrics_refresh_failures_count(),
            8,
            "eight independent fallible sections must each bump the counter exactly once"
        );
        assert_eq!(metrics.orders_created_value(), 11, "orders_created must freeze");
        assert_eq!(
            metrics.orders_partially_filled_value(),
            12,
            "orders_partially_filled must freeze"
        );
        assert_eq!(metrics.projection_backlog_value(), 13, "projection_backlog must freeze");
        assert_eq!(metrics.projection_lag_seconds_value(), 14, "projection_lag must freeze");
        assert_eq!(metrics.capture_cursor_age_seconds_value(), 15, "cursor_age must freeze");
        assert_eq!(
            metrics.inference_market_states_value(),
            (16, 17, 18),
            "market-state buckets must freeze"
        );
        assert_eq!(
            metrics.inference_reference_price_lag_seconds_value(),
            19,
            "reference-price lag must freeze"
        );
        assert_eq!(metrics.inference_sweep_lag_seconds_value(), 20, "sweep lag must freeze");
        assert_eq!(
            metrics.inference_order_counts_value(),
            (21, 22, 23, 24),
            "order-status buckets must freeze"
        );
        assert_eq!(metrics.inference_wedged_books_value(), 25, "wedged-books must freeze");
    }
}
