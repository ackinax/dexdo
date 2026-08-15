// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test for the inference-market state + staleness metric queries.
// Gated on TEST_DATABASE_URL: when unset the test prints a skip notice and
// returns.
//
//   cargo test -p dodex-infrastructure --test inference_market_metrics

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

// The state-count queries are whole-table aggregates. Under nextest's
// process-per-test model many sibling tests insert into `inference_markets`
// concurrently, so a whole-table count delta is not reproducible. This test
// asserts exact per-bucket counts and staleness lower bounds scoped to its own
// seeded addresses (the `*_for` siblings) — immune to concurrent writers. The
// whole-table production variants run the very same shared SELECT consts
// (`MARKET_STATE_COUNTS_SELECT`, `STALENESS_SELECT`), so the scoped asserts
// cover their predicates without a whole-table call. Staleness stays a lower
// bound because wall-clock time keeps passing between the seed and the query.
// model_hash is left NULL on every row: the UNIQUE partial index
// `inference_markets_model_hash_idx` ignores NULLs, so distinct rows never collide.
#[tokio::test]
async fn state_counts_and_staleness_reflect_inserted_rows() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let discovering = "inf_metrics_test.discovering";
    let visible = "inf_metrics_test.visible";
    let failing = "inf_metrics_test.failing";

    let addrs = vec![discovering.to_string(), visible.to_string(), failing.to_string()];
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("purge");

    // discovering: seeded, never reconciled, never failed.
    sqlx::query("insert into inference_markets (orderbook_address) values ($1)")
        .bind(discovering)
        .execute(&pool)
        .await
        .expect("insert discovering");
    // visible: reconciled, with a deliberately stale price (1000s) and sweep (500s).
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, last_reconciled_at, reference_price_at, last_swept_at)
           values ($1, now(), now() - interval '1000 seconds', now() - interval '500 seconds')"#,
    )
    .bind(visible)
    .execute(&pool)
    .await
    .expect("insert visible");
    // failing: invisible, has recorded a failure.
    sqlx::query(
        "insert into inference_markets (orderbook_address, last_reconcile_failed_at) values ($1, now())",
    )
    .bind(failing)
    .execute(&pool)
    .await
    .expect("insert failing");

    // Scoped to the three addresses seeded (and purged) above: exactly one row
    // lands in each bucket, pinning the CASE boundaries. Scoping makes this immune
    // to the concurrent sibling inserts a whole-table delta cannot exclude.
    let scoped = repo.inference_market_state_counts_for(&addrs).await.expect("scoped state counts");
    assert_eq!(scoped, (1, 1, 1), "each seeded row lands in exactly its own bucket");

    // Scoped to the same seeded addresses, only the `visible` row qualifies
    // (`last_reconciled_at is not null`), so both ages come from exactly the
    // timestamps seeded above: at least 1000s / 500s, plus whatever wall-clock
    // time passed since the insert. This is the positive scoped case proving
    // `STALENESS_SELECT` did not drift from the whole-table behaviour.
    let (price_lag, sweep_lag) =
        repo.inference_staleness_seconds_for(&addrs).await.expect("scoped staleness");
    assert!(price_lag >= 1000, "price_lag {price_lag} should be >= 1000");
    assert!(sweep_lag >= 500, "sweep_lag {sweep_lag} should be >= 500");

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("cleanup");
}

// The status counts are whole-table aggregates, so — like the market-state test
// above — this asserts exact per-bucket counts scoped to its own orderbook_address
// (`inference_order_status_counts_for`), immune to concurrent sibling inserts.
// Orders use a test-unique orderbook_address; the seed uses asymmetric per-bucket
// counts (1 open, 2 filled, 3 cancelled, 4 expired) so the scoped assert catches a
// swapped-column or mis-filtered bucket (e.g. `expired` counting `CANCELLED` rows)
// that a uniform seed like (1, 1, 1, 1) would let through.
#[tokio::test]
async fn order_status_counts_reflect_inserted_rows() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let ob = "inf_metrics_test.orders";
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("purge");

    // Asymmetric counts per bucket. Only the NOT NULL columns are set; order_id
    // is the per-book PK component.
    let seed = [
        (1i64, "OPEN"),
        (2, "FILLED"),
        (3, "FILLED"),
        (4, "CANCELLED"),
        (5, "CANCELLED"),
        (6, "CANCELLED"),
        (7, "EXPIRED"),
        (8, "EXPIRED"),
        (9, "EXPIRED"),
        (10, "EXPIRED"),
    ];
    for (order_id, status) in seed {
        sqlx::query(
            r#"insert into inference_orders
                   (orderbook_address, order_id, is_buy, price, amount_initial,
                    amount_remaining, status, last_chain_order)
               values ($1, $2, true, 100, 100, 100, $3, $2::text)"#,
        )
        .bind(ob)
        .bind(order_id)
        .bind(status)
        .execute(&pool)
        .await
        .expect("insert order");
    }

    // Scoped to this test's orderbook_address: exact asymmetric counts, immune
    // to the concurrent sibling inserts a whole-table delta cannot exclude.
    let scope = vec![ob.to_string()];
    let scoped = repo.inference_order_status_counts_for(&scope).await.expect("scoped order counts");
    assert_eq!(scoped, (1, 2, 3, 4), "each seeded order lands in exactly its own status bucket");

    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn staleness_for_an_empty_scope_is_zero_zero() {
    // IX-MET-04: `unwrap_or(0)` on the empty set is documented (indexer.md) but was
    // never asserted. A scoped probe over an address that has no rows is the only
    // concurrency-safe way to observe it in the shared test database.
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let scope = vec!["0:staleness_empty_scope_never_seeded".to_string()];
    let (price_lag, sweep_lag) =
        repo.inference_staleness_seconds_for(&scope).await.expect("staleness_for");
    assert_eq!((price_lag, sweep_lag), (0, 0), "empty scope must read as zero lag, not an error");
}

#[tokio::test]
async fn superseded_failed_row_excluded_from_failing_bucket() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let active = "inf_metrics_test.failing_active";
    let superseded = "inf_metrics_test.failing_superseded";
    let addrs = vec![active.to_string(), superseded.to_string()];
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .unwrap();

    // Active failing row -> counts toward failing.
    sqlx::query("insert into inference_markets (orderbook_address, last_reconcile_failed_at) values ($1, now())")
        .bind(active).execute(&pool).await.unwrap();
    // Superseded failed row -> excluded by `superseded_at is null`.
    sqlx::query("insert into inference_markets (orderbook_address, last_reconcile_failed_at, superseded_at) values ($1, now(), now())")
        .bind(superseded).execute(&pool).await.unwrap();

    // Scoped to just these two addresses, so a concurrent sibling's failing rows
    // cannot perturb the count: exactly one failing row (the active one).
    let (_d, _v, f) = repo.inference_market_state_counts_for(&addrs).await.unwrap();
    assert_eq!(f, 1, "only the non-superseded failing row counts toward the failing bucket");

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .unwrap();
}

/// `event_type`/`decoded` are left NULL — the wedged-books predicate only cares
/// about `src_address` and `processed_at is null`, matching the read gate's
/// arm-2, which is deliberately wider than the projection-pending predicate.
async fn insert_unprojected_raw_event(pool: &PgPool, msg_id: &str, src_address: &str) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $1, now(), $2, null, null, '{}'::jsonb, null)
           on conflict (msg_id) do nothing"#,
    )
    .bind(msg_id)
    .bind(src_address)
    .execute(pool)
    .await
    .expect("insert unprojected raw_events row");
}

// Pins the wedged-book predicate's exact qualifying set among four seeded
// addresses (wedged / clean / unreconciled / superseded), scoped via
// `inference_wedged_book_addresses` so a concurrent writer elsewhere in the
// shared test DB cannot perturb the result the way a whole-table count would.
// Asserting the exact set — not a count or a delta — pins WHICH book
// qualifies: inverting any single clause of the predicate (visibility,
// `superseded_at is null`, or the `exists(unprojected)` check) makes a
// different one of the four addresses appear or disappear, so every inversion
// changes this assertion's outcome. `inference_wedged_book_addresses` runs the
// same `WEDGED_BOOKS_WHERE` that `inference_wedged_books_count` runs
// whole-table (see indexer_repo.rs), so this also exercises the real
// production predicate rather than a parallel test-only copy.
#[tokio::test]
async fn wedged_books_count_reflects_unprojected_events_under_visible_books() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let wedged = "inf_metrics_test.wedged";
    let clean = "inf_metrics_test.wedged_clean";
    let unreconciled = "inf_metrics_test.wedged_unreconciled";
    let superseded = "inf_metrics_test.wedged_superseded";
    let addrs = vec![
        wedged.to_string(),
        clean.to_string(),
        unreconciled.to_string(),
        superseded.to_string(),
    ];

    sqlx::query("delete from raw_events where src_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("purge raw_events");
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("purge inference_markets");

    // wedged: visible book with an unprojected raw_events row under its address -> counted.
    sqlx::query(
        "insert into inference_markets (orderbook_address, last_reconciled_at) values ($1, now())",
    )
    .bind(wedged)
    .execute(&pool)
    .await
    .expect("insert wedged market");
    insert_unprojected_raw_event(&pool, "inf_metrics_test.wedged.evt1", wedged).await;

    // clean: visible book with no unprojected event -> not counted.
    sqlx::query(
        "insert into inference_markets (orderbook_address, last_reconciled_at) values ($1, now())",
    )
    .bind(clean)
    .execute(&pool)
    .await
    .expect("insert clean market");

    // unreconciled: never visible (last_reconciled_at null), but has an unprojected
    // event under its address -> not counted (excluded by the visibility predicate).
    sqlx::query("insert into inference_markets (orderbook_address) values ($1)")
        .bind(unreconciled)
        .execute(&pool)
        .await
        .expect("insert unreconciled market");
    insert_unprojected_raw_event(&pool, "inf_metrics_test.wedged.evt2", unreconciled).await;

    // superseded: was visible, now superseded, still has an unprojected event -> not
    // counted (excluded by superseded_at, same as the failing-bucket exclusion).
    sqlx::query(
        "insert into inference_markets (orderbook_address, last_reconciled_at, superseded_at) \
         values ($1, now(), now())",
    )
    .bind(superseded)
    .execute(&pool)
    .await
    .expect("insert superseded market");
    insert_unprojected_raw_event(&pool, "inf_metrics_test.wedged.evt3", superseded).await;

    // Scoped to just these four addresses: exactly `wedged` qualifies. Inverting
    // the visibility clause would let `unreconciled` in; inverting `superseded_at
    // is null` would let `superseded` in; inverting the `exists(unprojected)`
    // check would let `clean` in (and drop `wedged`) — any single-clause
    // inversion changes this set.
    let qualifying =
        repo.inference_wedged_book_addresses(&addrs).await.expect("wedged book addresses");
    assert_eq!(
        qualifying,
        vec![wedged.to_string()],
        "exactly the wedged book should satisfy the predicate among the four seeded addresses"
    );

    // Smoke-test the production (whole-table) path on the same predicate: it
    // must at least see our wedged book. Not an exact/delta count — other rows
    // in the shared test DB are expected and a concurrent writer could add or
    // remove wedged books elsewhere at any moment.
    let whole_table = repo.inference_wedged_books_count().await.expect("wedged count");
    assert!(
        whole_table >= 1,
        "whole-table wedged count {whole_table} should include our wedged book"
    );

    sqlx::query("delete from raw_events where src_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("cleanup raw_events");
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("cleanup inference_markets");
}

// The three whole-table queries are what production actually calls
// (`metrics_refresh.rs`); the `_for` siblings exist so the tests in this file can
// assert exact counts without a concurrent writer perturbing them. They share only
// the bucket predicates, through their consts — the `from` clause, the `unwrap_or(0)`
// and the tuple decode are written twice and were tested once. A swapped tuple
// element or a wrong `from` in the whole-table copy is invisible to every scoped
// test, and freeze-on-error (`metrics_refresh`) turns it into a plausible stale
// number on the dashboard rather than a visible gap.
//
// The assert is AGREEMENT, not an exact count. The scoped result is a subset of the
// whole-table one by construction, so `whole >= scoped` holds bucket for bucket at
// any moment, whatever a sibling is writing. That is weak on a uniform seed and
// sharp on this one: the seeded buckets differ from each other, so exchanging two
// tuple elements makes the whole-table read contradict the scoped read it must
// contain.
#[tokio::test]
async fn whole_table_metric_queries_agree_with_their_scoped_siblings() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let visible = "inf_whole_table.visible";
    let failing = "inf_whole_table.failing";
    let ob = "inf_whole_table.orders";
    let addrs = vec![visible.to_string(), failing.to_string()];

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("purge markets");
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("purge orders");

    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, last_reconciled_at, reference_price_at, last_swept_at)
           values ($1, now(), now() - interval '900 seconds', now() - interval '400 seconds')"#,
    )
    .bind(visible)
    .execute(&pool)
    .await
    .expect("insert visible");
    sqlx::query(
        "insert into inference_markets (orderbook_address, last_reconcile_failed_at) values ($1, now())",
    )
    .bind(failing)
    .execute(&pool)
    .await
    .expect("insert failing");

    // Asymmetric per bucket (1 open, 2 filled, 0 cancelled, 1 expired), so a swapped
    // tuple element cannot hide behind equal counts.
    for (order_id, status) in [(1i64, "OPEN"), (2, "FILLED"), (3, "FILLED"), (4, "EXPIRED")] {
        sqlx::query(
            r#"insert into inference_orders
                   (orderbook_address, order_id, is_buy, price, amount_initial,
                    amount_remaining, status, last_chain_order)
               values ($1, $2, true, 100, 100, 100, $3, $2::text)"#,
        )
        .bind(ob)
        .bind(order_id)
        .bind(status)
        .execute(&pool)
        .await
        .expect("insert order");
    }

    let (dw, vw, fw) = repo.inference_market_state_counts().await.expect("whole-table states");
    let (ds, vs, fs) = repo.inference_market_state_counts_for(&addrs).await.expect("scoped states");
    assert!(
        dw >= ds && vw >= vs && fw >= fs,
        "whole-table state counts must contain the scoped ones bucket for bucket: \
         whole=({dw},{vw},{fw}) scoped=({ds},{vs},{fs})"
    );
    assert!(vw >= 1 && fw >= 1, "the seeded visible and failing rows must be visible whole-table");

    let (pw, sw) = repo.inference_staleness_seconds().await.expect("whole-table staleness");
    let (ps, ss) = repo.inference_staleness_seconds_for(&addrs).await.expect("scoped staleness");
    assert!(
        pw >= ps && sw >= ss,
        "whole-table staleness maximises over a superset, so it can only be larger: \
         whole=({pw},{sw}) scoped=({ps},{ss})"
    );
    // The two ages differ by 500s in the seed, which is what makes an exchanged pair
    // fail rather than pass unnoticed.
    assert!(ps >= 900 && ss >= 400, "the scoped read must see the ages seeded here: {ps}/{ss}");

    let scope = vec![ob.to_string()];
    let (ow, flw, cw, ew) = repo.inference_order_status_counts().await.expect("whole-table orders");
    let (os, fls, cs, es) =
        repo.inference_order_status_counts_for(&scope).await.expect("scoped orders");
    assert_eq!((os, fls, cs, es), (1, 2, 0, 1), "the scoped read pins this test's own seed");
    assert!(
        ow >= os && flw >= fls && cw >= cs && ew >= es,
        "whole-table status counts must contain the scoped ones bucket for bucket: \
         whole=({ow},{flw},{cw},{ew}) scoped=({os},{fls},{cs},{es})"
    );

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("cleanup markets");
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("cleanup orders");
}
