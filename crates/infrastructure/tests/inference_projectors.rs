// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference order-book projectors.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::inference_projectors::project_inference_event;
use dodex_infrastructure::inference_projectors::repair_expired_inference_orphan;
use dodex_infrastructure::inference_projectors::ExpiredOrphanOutcome;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

mod fixtures;

use fixtures::chain_bodies::INFERENCE_ORDER_PLACED;

// Call sites pass the full on-wire event name (e.g. "InferenceOrderPlaced");
// since v4.0.10 the inference book emits every event with an `Inference` prefix.
fn ev(event_name: &str, value: serde_json::Value) -> DecodedEvent {
    DecodedEvent {
        contract_kind: "InferenceOrderBook",
        event_type: format!("InferenceOrderBook.{event_name}"),
        event_name: event_name.to_string(),
        value,
    }
}
fn node(src: &str, chain_order: &str) -> EventNode {
    EventNode {
        msg_id: format!("m_{chain_order}"),
        msg_chain_order: Some(chain_order.to_string()),
        src: Some(src.to_string()),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: Some(serde_json::json!(1_700_000_000)),
    }
}
async fn project(
    tx: &mut Transaction<'_, Postgres>,
    e: &DecodedEvent,
    n: &EventNode,
) -> ProjectionOutcome {
    project_inference_event(tx, e, n).await.unwrap()
}

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

#[tokio::test]
async fn skeleton_insert_needs_only_orderbook_and_chain_time() {
    let Some(pool) = setup().await else { return };
    let ob = "0:skeleton_smoke_ob";
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // Skeleton: only the two seed columns. Must not violate NOT NULL anywhere.
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain)
         values ($1, to_timestamp(1700000000)) on conflict (orderbook_address) do nothing",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .expect("skeleton insert must succeed");
    let (reconciled, attempts): (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("select last_reconciled_at, reconcile_attempts from inference_markets where orderbook_address=$1")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert!(reconciled.is_none(), "skeleton must be invisible (last_reconciled_at NULL)");
    assert_eq!(attempts, 0);
}

#[tokio::test]
async fn order_placed_seeds_market_and_rests_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_op_seed_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"5","isBuy":true,"price":"100","ticks":"10","note":"0:note5",
        // A BUY carries the zero address on chain; only a SELL names a deal contract.
        "tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0" }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    // Market skeleton seeded, still invisible.
    let reconciled: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select last_reconciled_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(reconciled.is_none());
    // Order rests OPEN with full amount.
    let (status, init, rem, is_buy): (String, String, String, bool) = sqlx::query_as(
        "select status, amount_initial::text, amount_remaining::text, is_buy from inference_orders where orderbook_address=$1 and order_id=5")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), init.as_str(), rem.as_str(), is_buy), ("OPEN", "10", "10", true));
}

#[tokio::test]
async fn order_placed_replay_does_not_reset_partial_fill() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_op_replay_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({"orderId":"9","isBuy":false,"price":"7","ticks":"10","note":"0:n","tokenContract":"0:tc","deadline":"0","flags":"0"}),
    );
    project(&mut tx, &e, &node(ob, "co-1")).await;
    tx.commit().await.unwrap();
    // Simulate a partial fill landing (manually) then replay the placement.
    sqlx::query(
        "update inference_orders set amount_remaining=4 where orderbook_address=$1 and order_id=9",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &e, &node(ob, "co-1")).await; // replay
    tx.commit().await.unwrap();
    let rem: String = sqlx::query_scalar("select amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=9").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(rem, "4", "replay must not reset amount_remaining to full ticks");
}

#[tokio::test]
async fn placement_with_subscription_flag_marks_the_row() {
    let Some(pool) = setup().await else { return };
    let ob = "0:sub_flag";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();

    // FLAG_SUBSCRIPTION = 0x40 (contracts/airegistry/modifiers/modifiers.sol).
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
            "note": "0:buyer", "tokenContract": "0:0", "deadline": "0", "flags": "64"
        }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "a1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let is_sub: bool = sqlx::query_scalar(
        "select is_subscription from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(is_sub, "FLAG_SUBSCRIPTION в `flags` обязан ставить is_subscription");
}

#[tokio::test]
async fn placement_without_the_flag_is_not_a_subscription() {
    let Some(pool) = setup().await else { return };
    let ob = "0:sub_noflag";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();

    // FLAG_AON = 0x20 — установлен, но это не подписка.
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
            "note": "0:buyer", "tokenContract": "0:0", "deadline": "0", "flags": "32"
        }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "a1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let is_sub: bool = sqlx::query_scalar(
        "select is_subscription from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!is_sub, "выставлен другой бит — подпиской это не делает");
}

#[tokio::test]
async fn a_placement_replay_does_not_resurrect_an_expired_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_replay";
    clean(&pool, ob).await;
    let placed = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
            "note": "0:b", "tokenContract": "0:0", "deadline": "0", "flags": "0"
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &placed, &node(ob, "a1")).await;
    let expired = ev("InferenceOrderExpired", serde_json::json!({"orderId": "1"}));
    project(&mut tx, &expired, &node(ob, "a2")).await;
    // Тот же placement приходит повторно (перекрытие страниц захвата).
    project(&mut tx, &placed, &node(ob, "a1")).await;
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(status, "EXPIRED", "реплей размещения не имеет права открыть истёкший ордер");
    assert_eq!(rem, "10", "остаток не восстанавливается");
}

#[tokio::test]
async fn a_late_cancel_does_not_demote_an_expired_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_cancel";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
                "note": "0:b", "tokenContract": "0:0", "deadline": "0", "flags": "0"
            }),
        ),
        &node(ob, "a1"),
    )
    .await;
    project(
        &mut tx,
        &ev("InferenceOrderExpired", serde_json::json!({"orderId": "1"})),
        &node(ob, "a2"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceOrderCancelled",
            serde_json::json!({"orderId": "1", "refunded": "0", "note": "0:b"}),
        ),
        &node(ob, "a3"),
    )
    .await;
    tx.commit().await.unwrap();

    let (status, _) = status_rem(&pool, ob, 1).await;
    assert_eq!(status, "EXPIRED", "истечение уже терминально; отмена его не переписывает");
}

#[tokio::test]
async fn a_late_fill_does_not_revive_an_expired_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_fill";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
                "note": "0:b", "tokenContract": "0:0", "deadline": "0", "flags": "0"
            }),
        ),
        &node(ob, "a1"),
    )
    .await;
    // ВТОРАЯ НОГА ОБЯЗАТЕЛЬНА: `apply_inference_filled` возвращает Deferred, пока
    // хоть одна названная строка отсутствует. Без неё дефектный код тоже оставил бы
    // EXPIRED и остаток 10 — тест был бы зелёным, не дойдя до apply_filled_decrement.
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "2", "isBuy": false, "price": "5", "ticks": "10",
                "note": "0:s", "tokenContract": "0:tc", "deadline": "1", "flags": "0"
            }),
        ),
        &node(ob, "a2"),
    )
    .await;
    project(
        &mut tx,
        &ev("InferenceOrderExpired", serde_json::json!({"orderId": "1"})),
        &node(ob, "a3"),
    )
    .await;
    let outcome = project(
        &mut tx,
        &ev(
            "InferenceFilled",
            serde_json::json!({
                "makerId": "1", "takerId": "2", "ticks": "4", "clearingPrice": "5",
                "sellerTC": "0:tc_expfill", "buyerNote": "0:b", "sellerNote": "0:s"
            }),
        ),
        &node(ob, "co-expfill-4"),
    )
    .await;
    assert_eq!(outcome, ProjectionOutcome::Applied, "обе ноги на месте — филл обязан примениться");
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(status, "EXPIRED", "истечение терминально и для филла");
    assert_eq!(rem, "10", "остаток истёкшей строки филл не уменьшает");
}

// `place` (:721) хардкодит `deadline: "0"`, поэтому дедлайновым тестам нужен свой посев.
// `is_buy` параметром, а не константой: последовательный тест ниже ставит SELL-ногу, и
// у неё дедлайн обязан быть ненулевым (контракт отвергает GTC-оффер, `:1600`).
async fn place_with_deadline(
    pool: &sqlx::PgPool,
    ob: &str,
    order_id: &str,
    is_buy: bool,
    deadline: &str,
    chain_order: &str,
) {
    let mut tx = pool.begin().await.unwrap();
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": order_id, "isBuy": is_buy, "price": "5", "ticks": "10",
            "note": "0:b", "tokenContract": if is_buy { ZERO_ADDRESS } else { "0:tc" },
            "deadline": deadline, "flags": "0"
        }),
    );
    project(&mut tx, &e, &node(ob, chain_order)).await;
    tx.commit().await.unwrap();
}

fn refunded(order_id: &str) -> DecodedEvent {
    ev("InferenceRefunded", serde_json::json!({"orderId": order_id, "note": "0:b", "amount": "7"}))
}

fn expired_ev(order_id: &str) -> DecodedEvent {
    ev(
        "InferenceOrderExpired",
        serde_json::json!({"orderId": order_id, "isBuy": true, "note": "0:n", "tokenContract": ZERO_ADDRESS}),
    )
}

// ДВА ГАРДА, оба зелёные до правки, и оба обязательны: предикат «закрывать можно»
// состоит из двух конъюнктов, и каждый нужно прижать своим тестом.
//
// Общий смысл — ОТКАЗ закрывать строку: `_finalizeTaker` (`:1183`, `:1223`) шлёт
// рефанд тейкеру, чей дедлайн ещё не прошёл, и что с ордером случилось на самом
// деле, знает `InferenceFilled`, а не рефанд. Остаток IOC/MARKET закрывает phantom
// sweep, сверяясь с цепью.

// Конъюнкт 1: `deadline is not null`. GTC-бид приходит с нулём, проектор размещения
// нормализует его в SQL NULL.
#[tokio::test]
async fn a_filled_orphan_with_no_legs_still_records_the_seller() {
    let Some(pool) = setup().await else { return };
    let ob = "0:seller_direct";
    // `clean` чистит orders/trades/markets, но НЕ inference_deals, а upsert сделки
    // сохраняет первый ненулевой seller_note через coalesce. Общий ключ `0:tc`
    // делил бы строку с другими тестами, и под параллельным nextest результат
    // зависел бы от порядка. Ключ уникален и убирается за собой.
    let tc = "0:tc_seller_direct";
    clean(&pool, ob).await;
    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();

    // Ни одна нога не спроецирована — ровно случай orphan-repair. Продавец при
    // этом назван самим событием, значит теряться ему не за что.
    let e = ev(
        "InferenceFilled",
        serde_json::json!({
            "makerId": "1", "takerId": "2", "ticks": "3", "clearingPrice": "5",
            "sellerTC": tc, "buyerNote": "0:buyer", "sellerNote": "0:seller"
        }),
    );
    repair_expired_inference_orphan(&mut tx, &e, &node(ob, "co-sellerdirect-1")).await.unwrap();
    tx.commit().await.unwrap();

    let seller: Option<String> = sqlx::query_scalar(
        "select seller_note from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        seller.as_deref(),
        Some("0:seller"),
        "продавец приехал в событии — обход по ноге не нужен"
    );
    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_orphans_name_all_four_deferrable_types() {
    // Отложиться без родителя умеют Filled, Cancelled, Expired и Refunded.
    // Каждый обязан иметь собственный исход: `Nothing` означает «мы не знаем, что
    // потеряли», и в логе от него нет пользы.
    for (name, value) in [
        ("InferenceOrderExpired", serde_json::json!({"orderId": "9"})),
        ("InferenceRefunded", serde_json::json!({"orderId": "9", "note": "0:b", "amount": "1"})),
    ] {
        let Some(pool) = setup().await else { return };
        let ob = "0:orphan_types";
        clean(&pool, ob).await;
        let mut tx = pool.begin().await.unwrap();
        let outcome = repair_expired_inference_orphan(&mut tx, &ev(name, value), &node(ob, "a1"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_ne!(
            outcome,
            ExpiredOrphanOutcome::Nothing,
            "{name}: сирота этого типа обязан быть назван, а не списан молча"
        );
    }
}

#[tokio::test]
async fn a_uint256_price_arrives_as_decimal_not_hex() {
    // ГАРД. `clearingPrice` и `price` — uint256, декодер отдаёт их "0x"+64 hex
    // (`uint256_hex_to_decimal`). Все прочие фикстуры подают десятичное, где
    // старый и новый код неотличимы, — поэтому здесь hex.
    //
    // `expect`, а не `else { return }`: этот гард — единственное, что отличает
    // верный переход на DTO от порчи данных.
    let pool = setup().await.expect("hex-гард требует Postgres");
    let ob = "0:uint256_hex";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    // 0x…0101 = 257
    let hex257 = "0x0000000000000000000000000000000000000000000000000000000000000101";
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "1", "isBuy": false, "price": hex257, "ticks": "10",
                "note": "0:s", "tokenContract": "0:tc", "deadline": "1700009999", "flags": "0"
            }),
        ),
        &node(ob, "co-hex-1"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "2", "isBuy": true, "price": hex257, "ticks": "10",
                "note": "0:b", "tokenContract": ZERO_ADDRESS, "deadline": "0", "flags": "0"
            }),
        ),
        &node(ob, "co-hex-2"),
    )
    .await;
    let f = ev(
        "InferenceFilled",
        serde_json::json!({
            "makerId": "1", "takerId": "2", "ticks": "10", "clearingPrice": hex257,
            "sellerTC": "0:tc_hex", "buyerNote": "0:b", "sellerNote": "0:s"
        }),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-hex-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let price: String = sqlx::query_scalar(
        "select price::text from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(price, "257", "uint256 обязан доехать десятичным: numeric не примет hex");

    let traded: String =
        sqlx::query_scalar("select price::text from inference_trades where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(traded, "257", "цена сделки в ленте обязана быть десятичной");
}

#[tokio::test]
async fn a_fill_without_seller_tc_still_decrements_the_legs() {
    // ГАРД на снисходительность: строгое поле в DTO превратит дрейф ABI из
    // «сделка не связалась, есть warn» в «каждый филл отказывает вечно».
    let Some(pool) = setup().await else { return };
    let ob = "0:dto_lenient";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", false, "10", "co-lenient-1").await;
    place(&pool, &mut tx, ob, "2", true, "10", "co-lenient-2").await;
    let f = ev(
        "InferenceFilled",
        // sellerTC и sellerNote отсутствуют — форма payload'а до v4.0.33.
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"1","buyerNote":"0:b"}),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-lenient-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 1).await, ("FILLED".into(), "0".into()));
    assert_eq!(status_rem(&pool, ob, 2).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn a_refund_on_a_gtc_order_leaves_it_open() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_gtc";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "0", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        (status.as_str(), rem.as_str()),
        ("OPEN", "10"),
        "рефанд по GTC не различает исполненного тейкера и остаток IOC — угадывать нельзя"
    );
}

// Конъюнкт 2: `deadline <= chain_seconds`. БЕЗ ЭТОГО ТЕСТА реализация, проверяющая
// только `is not null`, проходит весь набор и при этом помечает `EXPIRED` живые
// ордера с будущим дедлайном. Случай не синтетический, а ровно вся SELL-сторона:
// оффер обязан нести ненулевой дедлайн (`InferenceOrderBook.sol:1600` отвергает
// нулевой как malformed), значит taker-only SELL из `:1223` всегда попадает сюда.
#[tokio::test]
async fn a_refund_before_a_future_deadline_leaves_the_order_open() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_future";
    clean(&pool, ob).await;
    // node() ставит created_at = 1_700_000_000; дедлайн заведомо позже.
    place_with_deadline(&pool, ob, "1", false, "1700009999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        (status.as_str(), rem.as_str()),
        ("OPEN", "10"),
        "ордер с ненаступившим дедлайном живой: закрыть его по рефанду — выдумать истечение"
    );
}

#[tokio::test]
async fn a_refund_past_the_deadline_expires_the_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_exp";
    clean(&pool, ob).await;
    // node() ставит created_at = 1_700_000_000; дедлайн на секунду раньше.
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, _) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        status, "EXPIRED",
        "continuation, истёкший до возобновления, эмитит только рефанд — статус выводится из дедлайна"
    );
}

// ГАРД, а не red-first тест: сегодня `InferenceRefunded` уходит в observability-арм
// и строку не трогает, поэтому проверка зелёная и до правки. Она существует, чтобы
// НОВЫЙ проектор не начал трогать терминальную строку — включая `updated_at`.
#[tokio::test]
async fn a_refund_over_a_filled_row_changes_nothing_at_all() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_filled";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "0", "a1").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("update inference_orders set status='FILLED', amount_remaining=0 where orderbook_address=$1")
        .bind(ob)
        .execute(&mut *tx)
        .await
        .unwrap();
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, _) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        status, "FILLED",
        "рефанд обслуживает и удаление dust — исполненную строку он не трогает"
    );
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, before, "no-op обязан быть полным: даже updated_at не двигается");
}

#[tokio::test]
async fn a_refund_without_its_parent_defers() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_orphan";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(&mut tx, &refunded("1"), &node(ob, "a1")).await,
        ProjectionOutcome::Deferred
    );
    tx.commit().await.unwrap();
}

// ГАРД, зелёный и до правки — и ЕДИНСТВЕННЫЙ тест, который краснеет, если кто-нибудь
// вернёт проектору ветку `CANCELLED` для непрошедшего дедлайна. Это ровно тот сценарий,
// из-за которого её здесь нет; расширение существующего
// `filled_defers_zero_writes_when_one_side_absent_then_applies_once` вставкой
// рефанда между `Deferred` и повтором.
//
// На цепи полностью исполненный тейкер-BUY даёт `Placed(2)` -> `Filled(1,2)` ->
// `Refunded(2, leftover)` (`:1183`; событие уходит даже при `leftover == 0`). Мейкерская
// нога потеряна на capture, поэтому fill откладывается, а дренаж идёт дальше
// (`indexer_repo.rs:522` строку не помечает и берёт следующую) — рефанд применяется
// РАНЬШЕ своего же fill'а.
#[tokio::test]
async fn a_refund_between_a_deferred_fill_and_its_retry_does_not_steal_the_terminal_status() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_seq";
    clean(&pool, ob).await;
    // Тейкер (id 2) спроецирован; мейкерская SELL-нога (id 1) — ещё нет.
    place_with_deadline(&pool, ob, "2", true, "0", "a1").await;
    let f = ev(
        "InferenceFilled",
        // sellerTC уникален по репозиторию: clean() не чистит inference_deals (PK — адрес TC).
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"5",
                           "sellerTC":"0:tc_refseq","buyerNote":"0:b","sellerNote":"0:s"}),
    );

    // 1. Fill откладывается — нулевые записи.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &f, &node(ob, "co-refseq-9")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();

    // 2. Рефанд тейкера дренируется следующим и обязан ничего не решить.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("2"), &node(ob, "a3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, _) = status_rem(&pool, ob, 2).await;
    assert_eq!(status, "OPEN", "рефанд закрыл строку, у которой висит неприменённый fill");

    // 3. Мейкерская нога приезжает, fill повторяется.
    place_with_deadline(&pool, ob, "1", false, "1700009999", "a4").await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &f, &node(ob, "co-refseq-9")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    assert_eq!(
        status_rem(&pool, ob, 2).await,
        ("FILLED".into(), "0".into()),
        "исполненный тейкер обязан прийти в FILLED: рефанд лишь вернул неистраченный escrow"
    );
}

// ГАРД на ПРЯМОЙ порядок — тот, что бывает на цепи. `_removeExpiredBid`
// (`InferenceOrderBook.sol:1143-1146`) зовёт `_refundAndRemove` и лишь ПОТОМ эмитит
// `InferenceOrderExpired`, то есть по `chain_order` рефанд всегда первый.
//
// Зубы у теста конкретные: закрой рефанд строку любым статусом, кроме `EXPIRED`
// (например `CANCELLED`, как в первой редакции этой задачи), — и пришедшее следом
// истечение уже ничего не поправит.
#[tokio::test]
async fn a_refund_before_its_expiry_event_leaves_expired_standing() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_then_exp";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    assert_eq!(
        project(&mut tx, &expired_ev("1"), &node(ob, "a3")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), swept_null),
        ("EXPIRED", true),
        "рефанд обязан закрыть просроченную строку ИМЕННО в EXPIRED — иначе истечение её не подберёт"
    );
}

// ГАРД на ОБРАТНЫЙ порядок: реплей и перестановка внутри батча. Здесь `updated_at`
// проверяется намеренно и не для красоты — статус-ассерт тут беззубый: строка уже
// `EXPIRED`, и предикат вида «не FILLED и не настоящая отмена» перезаписал бы её в
// тот же `EXPIRED`, оставив ассерт зелёным. Ловит подмену только полный no-op.
#[tokio::test]
async fn a_refund_after_its_expiry_event_changes_nothing_at_all() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_then_ref";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(&mut tx, &expired_ev("1"), &node(ob, "a2")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, after): (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "select status, updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "EXPIRED");
    assert_eq!(
        after, before,
        "`EXPIRED` обязан быть вне предиката рефанда: строка не тронута вовсе"
    );
}

#[tokio::test]
async fn order_cancelled_is_terminal_and_defers_when_absent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_cancel_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // Cancel with no prior placement => Deferred (zero writes).
    let mut tx = pool.begin().await.unwrap();
    let c = ev("InferenceOrderCancelled", serde_json::json!({"orderId":"2","refunded":"0"}));
    assert_eq!(project(&mut tx, &c, &node(ob, "co-1")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "select count(*) from inference_orders where orderbook_address=$1 and order_id=2",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
    // Place then cancel => CANCELLED, swept_at NULL.
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":"2","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"})),&node(ob,"co-2")).await;
    assert_eq!(project(&mut tx, &c, &node(ob, "co-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=2").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), swept_null), ("CANCELLED", true));
}

// The chain decides expiry, not the reader: a resting order whose deadline has
// passed keeps its OPEN status until InferenceOrderExpired arrives, and only then
// becomes EXPIRED. Nothing derives the status from `deadline` vs wall-clock.
// The one exception is a Refunded whose chain time is past this deadline: there the
// chain has already removed the order, and the deadline only says the cause was
// expiry — continuation expiry emits no InferenceOrderExpired at all.
#[tokio::test]
async fn order_expired_is_terminal_and_defers_when_absent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_expire_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // Expiry with no prior placement => Deferred (zero writes), same as a cancel.
    let mut tx = pool.begin().await.unwrap();
    let x = ev(
        "InferenceOrderExpired",
        serde_json::json!({"orderId":"3","isBuy":true,"note":"0:n","tokenContract":ZERO_ADDRESS}),
    );
    assert_eq!(project(&mut tx, &x, &node(ob, "xo-1")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "select count(*) from inference_orders where orderbook_address=$1 and order_id=3",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
    // A placed order stays OPEN while its deadline sits in the past ...
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":"3","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"1700000000","flags":"0"})),&node(ob,"xo-2")).await;
    tx.commit().await.unwrap();
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=3",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "a past deadline alone must not change the status");
    // ... and only the event makes it EXPIRED, leaving swept_at untouched.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &x, &node(ob, "xo-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=3").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), swept_null), ("EXPIRED", true));
}

// The common ordering, not an edge case: an order expires, the sweep notices it is
// gone from the book and provisionally marks it CANCELLED, and only then does the
// authoritative InferenceOrderExpired arrive. The event must win, or every expiry
// that the sweep outruns is recorded under the wrong terminal status. A real
// event-cancel (swept_at NULL) stays CANCELLED — the order was gone before it aged out.
#[tokio::test]
async fn expired_overrides_provisional_sweep_cancel_but_not_a_real_cancel() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_expire_override";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    for id in ["40", "41"] {
        project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":id,"isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"1700000000","flags":"0"})),&node(ob,&format!("eo-{id}"))).await;
    }
    tx.commit().await.unwrap();
    // 40: provisional sweep-cancel (swept_at set). 41: real event-cancel (swept_at NULL).
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=now() where orderbook_address=$1 and order_id=40").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=null where orderbook_address=$1 and order_id=41").bind(ob).execute(&pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    for id in ["40", "41"] {
        let x = ev(
            "InferenceOrderExpired",
            serde_json::json!({"orderId":id,"isBuy":true,"note":"0:n","tokenContract":ZERO_ADDRESS}),
        );
        assert_eq!(
            project(&mut tx, &x, &node(ob, &format!("ex-{id}"))).await,
            ProjectionOutcome::Applied
        );
    }
    tx.commit().await.unwrap();

    let (swept_status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=40").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (swept_status.as_str(), swept_null),
        ("EXPIRED", true),
        "a provisional sweep-cancel must yield to the authoritative expiry"
    );
    let real_status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=41",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(real_status, "CANCELLED", "a real event-cancel stays terminal");
}

#[tokio::test]
async fn observability_event_seeds_market_only() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_obs_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    // Раньше здесь стоял `InferenceRefunded` БЕЗ `orderId` — событие, которого не
    // бывает. Теперь у рефанда есть свой проектор со строгим разбором id, и такой
    // payload уронил бы `project`. Смысл теста (любое inference-событие сеет скелет
    // рынка) держит любой из оставшихся observability-типов.
    let r =
        ev("InferenceExecuted", serde_json::json!({"ticks":"1","clearingPrice":"1","cost":"1"}));
    assert_eq!(project(&mut tx, &r, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let m: i64 =
        sqlx::query_scalar("select count(*) from inference_markets where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    let o: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((m, o), (1, 0), "observability seeds the market but creates no order");
}

#[tokio::test]
async fn routes_by_event_type_when_event_name_is_empty() {
    // The reprojection loop reconstructs DecodedEvent with event_name EMPTY (only
    // event_type is persisted). This guards against routing on event_name, which
    // would send every live captured row to the seed-only path.
    let Some(pool) = setup().await else { return };
    let ob = "0:t_empty_name";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let loop_shaped = DecodedEvent {
        contract_kind: "",
        event_name: String::new(), // <-- as the live loop builds it
        event_type: "InferenceOrderBook.InferenceOrderPlaced".to_string(),
        value: serde_json::json!({"orderId":"7","isBuy":true,"price":"1","ticks":"3","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
    };
    assert_eq!(project(&mut tx, &loop_shaped, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=7",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "empty event_name must still reach the OrderPlaced handler");
}

// ---- Orphan dead-letter helpers and test ----

use dodex_infrastructure::indexer_repo::IndexerRepository;

// ingest_age_secs => raw_events.created_at; chain_age_secs => created_at_chain (independent),
// so a test can make a row freshly-ingested yet ancient on chain.
#[allow(clippy::too_many_arguments)]
// Test helper with 8 intentional knobs (pool, msg, chain_order, ingest_age, chain_age, ob, event_type, decoded) for orphan tests
async fn insert_raw(
    pool: &sqlx::PgPool,
    msg: &str,
    co: &str,
    ingest_age_secs: i64,
    chain_age_secs: i64,
    ob: &str,
    event_type: &str,
    decoded: serde_json::Value,
) {
    sqlx::query(
        "insert into raw_events (msg_id, chain_order, created_at_chain, created_at, src_address, dst_address, event_type, body_json, decoded)
         values ($1, $2, now() - make_interval(secs => $3), now() - make_interval(secs => $4), $5, null, $6, '{}'::jsonb, $7::jsonb)
         on conflict (msg_id) do nothing")
        .bind(msg).bind(co).bind(chain_age_secs as f64).bind(ingest_age_secs as f64).bind(ob).bind(event_type).bind(decoded.to_string())
        .execute(pool).await.unwrap();
}
async fn raw_processed(pool: &sqlx::PgPool, msg: &str) -> bool {
    sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "select processed_at from raw_events where msg_id=$1",
    )
    .bind(msg)
    .fetch_one(pool)
    .await
    .unwrap()
    .is_some()
}
// Upsert the capture-stream cursor's `at_head` flag. The orphan dead-letter only
// fires once capture has drained to head; tests use a unique stream (via
// `with_capture_stream`) so they never race the shared live `blockchain_events` row.
async fn set_cursor_at_head(pool: &sqlx::PgPool, stream: &str, at_head: bool) {
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head, updated_at)
           values ($1, 'x', $2, now())
         on conflict (stream_name) do update set at_head = excluded.at_head, updated_at = now()",
    )
    .bind(stream)
    .bind(at_head)
    .execute(pool)
    .await
    .unwrap();
}
async fn order_amount_status(
    pool: &sqlx::PgPool,
    ob: &str,
    order_id: &str,
) -> Option<(i64, String)> {
    sqlx::query_as::<_, (i64, String)>(
        "select amount_remaining::bigint, status from inference_orders
          where orderbook_address=$1 and order_id=$2::numeric",
    )
    .bind(ob)
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn expired_orphans_dropped_all_four_types_using_ingest_age_not_chain_time() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_ob";
    sqlx::query("delete from raw_events where chain_order like '00orphan-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let filled = serde_json::json!({"makerId":"900","takerId":"901","ticks":"1","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"});
    let cancel = serde_json::json!({"orderId":"902","refunded":"0"});
    let expired =
        serde_json::json!({"orderId":"903","isBuy":true,"note":"0:b","tokenContract":ZERO_ADDRESS});
    let refunded = serde_json::json!({"orderId":"904","note":"0:b","amount":"1"});
    // (a)-(b'') aged-ingest orphans of ALL FOUR deferrable types => dropped.
    //
    // ЭТО ЗЕЛЁНЫЙ ГАРД, а не red-first тест. Гейт `is_expired_inference_orphan`
    // пропускает любой `InferenceOrderBook.*`, а оба пути дренажа матчат `Ok(_)`,
    // то есть помечают строку processed даже при исходе `Nothing`. Значит новые
    // типы дренируются и без арм'ов. Ценность в другом: тест покраснеет, если гейт
    // когда-нибудь сузят обратно до Filled/Cancelled — строки станут pending навсегда.
    insert_raw(
        &pool,
        "orphan-fill",
        "00orphan-a",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled.clone(),
    )
    .await;
    insert_raw(
        &pool,
        "orphan-cancel",
        "00orphan-b",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceOrderCancelled",
        cancel.clone(),
    )
    .await;
    insert_raw(
        &pool,
        "orphan-expired",
        "00orphan-b1",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceOrderExpired",
        expired.clone(),
    )
    .await;
    insert_raw(
        &pool,
        "orphan-refund",
        "00orphan-b2",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceRefunded",
        refunded.clone(),
    )
    .await;
    // (c) FRESH ingest but ANCIENT created_at_chain (1 day) => NOT dropped — cutoff uses ingest age, not chain time.
    insert_raw(
        &pool,
        "orphan-oldchain",
        "00orphan-c",
        0,
        86400,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled.clone(),
    )
    .await;
    // (d) fresh ingest, fresh chain => NOT dropped (normal short deferral).
    insert_raw(
        &pool,
        "orphan-fresh",
        "00orphan-d",
        0,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled.clone(),
    )
    .await;

    // Orphan dead-lettering only fires once capture has reached head.
    let stream = "orphan_drop_athead_stream";
    set_cursor_at_head(&pool, stream, true).await;
    IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00orphan-"), Some("00orphan-z"))
        .await
        .unwrap();

    assert!(raw_processed(&pool, "orphan-fill").await, "aged Filled orphan must be dropped");
    assert!(
        raw_processed(&pool, "orphan-cancel").await,
        "aged OrderCancelled orphan must be dropped"
    );
    assert!(
        raw_processed(&pool, "orphan-expired").await,
        "aged OrderExpired orphan must be dropped — сужение гейта оставило бы строку pending навсегда"
    );
    assert!(
        raw_processed(&pool, "orphan-refund").await,
        "aged Refunded orphan must be dropped — сужение гейта оставило бы строку pending навсегда"
    );
    assert!(!raw_processed(&pool, "orphan-oldchain").await,
        "old created_at_chain but fresh ingest => NOT dropped (proves raw_events.created_at, not chain time)");
    assert!(!raw_processed(&pool, "orphan-fresh").await, "fresh ingest => stays pending");
    // Dead-letter writes no order row.
    let n: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0);

    // Cleanup residual pending rows so they do not pollute other tests that
    // query max_pending_chain_order / has_pending_above globally.
    sqlx::query("delete from raw_events where chain_order like '00orphan-%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_orphan_not_dropped_until_capture_at_head() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_nh_ob";
    let stream = "orphan_not_athead_stream";
    sqlx::query("delete from raw_events where chain_order like '00orphnh-%'")
        .execute(&pool)
        .await
        .unwrap();
    let filled = serde_json::json!({"makerId":"800","takerId":"801","ticks":"1","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"});
    // Aged ingest (1h) — well past the 60s cutoff — so only the at_head gate decides.
    insert_raw(
        &pool,
        "orphnh-fill",
        "00orphnh-a",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled,
    )
    .await;

    let repo = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60));

    // at_head = false: a missing parent may still be ahead in the backfill, so the
    // aged orphan must NOT be declared permanently dropped yet.
    set_cursor_at_head(&pool, stream, false).await;
    repo.reproject_pending_from(50, Some("00orphnh-"), Some("00orphnh-z")).await.unwrap();
    assert!(
        !raw_processed(&pool, "orphnh-fill").await,
        "aged orphan must stay pending while capture is still backfilling (at_head=false)"
    );

    // Same row, same age; only at_head flips to true — now it is dead-lettered.
    set_cursor_at_head(&pool, stream, true).await;
    repo.reproject_pending_from(50, Some("00orphnh-"), Some("00orphnh-z")).await.unwrap();
    assert!(
        raw_processed(&pool, "orphnh-fill").await,
        "once capture reaches head, the aged orphan is dead-lettered"
    );

    sqlx::query("delete from raw_events where chain_order like '00orphnh-%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_filled_orphan_decrements_present_leg() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_leg_ob";
    let stream = "orphan_leg_athead_stream";
    sqlx::query("delete from raw_events where chain_order like '00orphld-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // The dead-letter repair below mints a global-PK inference_trades row keyed on the
    // raw event's chain order — clean it up like the other tables so a re-run starts fresh.
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Seed a resting BUY maker (id 700) with 10 ticks of depth via the real placement projector.
    let mut tx = pool.begin().await.unwrap();
    let placed = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId":"700","isBuy":true,"price":"5","ticks":"10","note":"0:n",
            "tokenContract":ZERO_ADDRESS,
            "deadline":"0","flags":"0",
        }),
    );
    assert_eq!(
        project(&mut tx, &placed, &node(ob, "00seed-700")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    assert_eq!(order_amount_status(&pool, ob, "700").await, Some((10, "OPEN".into())));

    // Aged Filled orphan: maker 700 is present and resting; taker 701's OrderPlaced was dropped.
    let filled = serde_json::json!({"makerId":"700","takerId":"701","ticks":"3","clearingPrice":"5","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"});
    insert_raw(
        &pool,
        "orphld-fill",
        "00orphld-a",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled,
    )
    .await;

    set_cursor_at_head(&pool, stream, true).await;
    IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00orphld-"), Some("00orphld-z"))
        .await
        .unwrap();

    // The orphan is dead-lettered...
    assert!(raw_processed(&pool, "orphld-fill").await, "aged Filled orphan dead-lettered");
    // ...but the present maker's depth is corrected (10 - 3 = 7), not left permanently stale.
    assert_eq!(
        order_amount_status(&pool, ob, "700").await,
        Some((7, "OPEN".into())),
        "present resting leg decremented by the fill before the drop"
    );
    // The missing taker leg is not fabricated.
    assert_eq!(order_amount_status(&pool, ob, "701").await, None, "missing leg is not created");

    sqlx::query("delete from raw_events where chain_order like '00orphld-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_range_event_orphan_dead_letters_like_the_books_own() {
    // `OracleEventList.RangeEventAdded` откладывается, пока нет родительского
    // `oracle_events`. Родители эмитятся oracle-списком, который мог быть
    // развёрнут ДО старта курсора захвата этого развёртывания — тогда они лежат
    // вне захваченной истории и не придут никогда. Сам RangeEventAdded строку не
    // создаёт, он аннотирует чужую, так что ждать нечего. Без финального исхода
    // строка pending навсегда и держит backlog наблюдателя выше нуля.
    let Some(pool) = setup().await else { return };
    let oel = "0:t_range_orphan_list";
    sqlx::query("delete from raw_events where chain_order like '00rgoph-%'")
        .execute(&pool)
        .await
        .unwrap();
    // Возраст приёма 3600 с — заведомо больше отсечки в 60 с.
    insert_raw(
        &pool,
        "rgoph-range",
        "00rgoph-a",
        3600,
        0,
        oel,
        "OracleEventList.RangeEventAdded",
        serde_json::json!({"eventId": "0x2a", "ob": "0:t_range_orphan_book", "bounds": []}),
    )
    .await;

    let stream = "range_orphan_stream";
    set_cursor_at_head(&pool, stream, true).await;
    let stats = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00rgoph-"), Some("00rgoph-z"))
        .await
        .unwrap();

    assert_eq!(stats.applied, 1, "dead-letter засчитывается как applied, как и у книги");
    assert!(
        raw_processed(&pool, "rgoph-range").await,
        "RangeEventAdded старше отсечки обязан получить финальный исход: \
         иначе он pending навсегда и роняет наблюдателя"
    );

    sqlx::query("delete from raw_events where chain_order like '00rgoph-%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_prediction_orphan_is_never_dead_lettered_however_old() {
    // ГАРД ОБРАТНОЙ СТОРОНЫ. Без него положительный тест выше не отличает
    // allow-list от снятого префиксного условия — а снятие было бы регрессом:
    // `projectors.rs` откладывает в четырнадцати местах, и почти все ждут того,
    // что законно приходит позже (`PMPDeployed` ждёт строку в `ref_tokens`,
    // `TimingsSet`/`Resolved` ждут свой `PMPDeployed`). При проде-отсечке в 30
    // минут их dead-letter убивает рынок молча и навсегда: строка помечается
    // processed и не переспрашивается (IX-FAIL-06).
    //
    // `OracleEventList.EventAdded` взят намеренно: тот же контракт, что у
    // разрешённого `RangeEventAdded`. Тест поэтому проверяет список по ПОЛНОМУ
    // имени, а не по префиксу контракта.
    let Some(pool) = setup().await else { return };
    let oel = "0:t_pred_orphan_list";
    sqlx::query("delete from raw_events where chain_order like '00prdph-%'")
        .execute(&pool)
        .await
        .unwrap();
    insert_raw(
        &pool,
        "prdph-added",
        "00prdph-a",
        3600,
        0,
        oel,
        "OracleEventList.EventAdded",
        serde_json::json!({
            "eventId": "0x2a", "eventName": "probe", "oracleFee": "0", "deadline": "0"
        }),
    )
    .await;

    let stream = "pred_orphan_stream";
    set_cursor_at_head(&pool, stream, true).await;
    let stats = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00prdph-"), Some("00prdph-z"))
        .await
        .unwrap();

    // `deferred`, а не просто «строка ещё pending»: ошибка разбора payload'а
    // тоже оставила бы строку pending, и тест зеленел бы по неверной причине.
    assert_eq!(stats.deferred, 1, "строка обязана быть именно ОТЛОЖЕНА, а не упасть с Err");
    assert!(
        !raw_processed(&pool, "prdph-added").await,
        "не-разрешённый тип обязан остаться отложенным независимо от возраста: \
         его родитель приходит законно позже, и dead-letter убил бы рынок молча"
    );

    sqlx::query("delete from raw_events where chain_order like '00prdph-%'")
        .execute(&pool)
        .await
        .unwrap();
}

// ---- Filled handler helpers ----

async fn place(
    pool: &sqlx::PgPool,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ob: &str,
    id: &str,
    is_buy: bool,
    ticks: &str,
    co: &str,
) {
    let _ = pool; // place via the projector for realism
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": id, "isBuy": is_buy, "price": "1", "ticks": ticks, "note": "0:n",
            // A BUY carries the zero address on chain; only a SELL names a deal contract.
            "tokenContract": if is_buy { ZERO_ADDRESS } else { "0:tc" },
            "deadline": "0","flags":"0",
        }),
    );
    project(tx, &e, &node(ob, co)).await;
}
async fn clean(pool: &sqlx::PgPool, ob: &str) {
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}
async fn status_rem(pool: &sqlx::PgPool, ob: &str, id: i64) -> (String, String) {
    sqlx::query_as("select status, amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=$2")
        .bind(ob).bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn filled_closes_sell_offer_and_zeroes_buy_taker() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_both";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", false, "10", "co-1").await; // SELL maker
    place(&pool, &mut tx, ob, "2", true, "10", "co-2").await; // BUY taker
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    // Applied Filled mints a global-PK inference_trades row keyed on this chain order —
    // must stay unique repo-wide, not just within this test.
    assert_eq!(project(&mut tx, &f, &node(ob, "co-fillboth-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 1).await, ("FILLED".into(), "0".into())); // SELL one-deal
    assert_eq!(status_rem(&pool, ob, 2).await, ("FILLED".into(), "0".into())); // BUY taker zeroed
}

#[tokio::test]
async fn buy_maker_fills_across_deals_to_filled_at_zero() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_across";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "10", true, "10", "co-1").await; // BUY maker
    place(&pool, &mut tx, ob, "11", false, "6", "co-2").await; // SELL taker A
    place(&pool, &mut tx, ob, "12", false, "4", "co-3").await; // SELL taker B
    project(&mut tx,&ev("InferenceFilled",serde_json::json!({"makerId":"10","takerId":"11","ticks":"6","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"})),&node(ob,"co-fillacross-4")).await; // trade_id unique repo-wide
    tx.commit().await.unwrap();
    // Read via the pool only AFTER commit — a separate pooled connection cannot see uncommitted rows.
    assert_eq!(status_rem(&pool, ob, 10).await, ("OPEN".into(), "4".into())); // committed partial
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceFilled",serde_json::json!({"makerId":"10","takerId":"12","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"})),&node(ob,"co-fillacross-5")).await; // trade_id unique repo-wide
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 10).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn filled_defers_zero_writes_when_one_side_absent_then_applies_once() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_defer";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "20", false, "5", "co-1").await; // only the maker exists
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"20","takerId":"21","ticks":"5","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-2")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    assert_eq!(
        status_rem(&pool, ob, 20).await,
        ("OPEN".into(), "5".into()),
        "present side must NOT be decremented"
    );
    // taker arrives, replay applies exactly once.
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "21", true, "5", "co-3").await;
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    assert_eq!(project(&mut tx, &f, &node(ob, "co-filldefer-4")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 20).await, ("FILLED".into(), "0".into()));
    assert_eq!(status_rem(&pool, ob, 21).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn filled_overrides_provisional_sweep_cancel_and_resets_discovery_cursor() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_override";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "30", true, "10", "co-1").await; // BUY maker
    place(&pool, &mut tx, ob, "31", false, "4", "co-2").await; // SELL taker
    tx.commit().await.unwrap();
    // Simulate a provisional sweep-cancel of the BUY maker, and set the book in
    // discovery with a non-null sweep_cursor mid-cycle.
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=now() where orderbook_address=$1 and order_id=30").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("update inference_markets set sweep_cursor=99, last_reconciled_at=null where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"30","takerId":"31","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    assert_eq!(
        project(&mut tx, &f, &node(ob, "co-filloverride-3")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    // Override: maker reopened OPEN with remaining 6, swept_at cleared.
    let (status, rem): (String, String) = status_rem(&pool, ob, 30).await;
    let swept_null: bool = sqlx::query_scalar(
        "select swept_at is null from inference_orders where orderbook_address=$1 and order_id=30",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), rem.as_str(), swept_null), ("OPEN", "6", true));
    // Discovery cursor reset so the reopened low id is re-checked before stamping.
    let cursor: Option<String> = sqlx::query_scalar(
        "select sweep_cursor::text from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cursor.is_none(), "discovery sweep_cursor must reset to NULL on override");
    // The first-tick visibility-stamp guard requires sweep_override_seq to bump.
    let seq: i64 = sqlx::query_scalar(
        "select sweep_override_seq from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        seq, 1,
        "override during discovery must bump sweep_override_seq from its default 0 to 1"
    );
}

#[tokio::test]
async fn filled_after_real_cancel_is_terminal_no_override() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_realcancel";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "40", true, "10", "co-1").await;
    place(&pool, &mut tx, ob, "41", false, "4", "co-2").await;
    tx.commit().await.unwrap();
    // Real event-cancel: CANCELLED + swept_at NULL.
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=null where orderbook_address=$1 and order_id=40").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"40","takerId":"41","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    project(&mut tx, &f, &node(ob, "co-fillrealcancel-3")).await;
    tx.commit().await.unwrap();
    assert_eq!(
        status_rem(&pool, ob, 40).await,
        ("CANCELLED".into(), "10".into()),
        "real cancel stays terminal, remainder preserved"
    );
    // FULL no-op: a late Filled arriving after the real cancel must not advance the
    // terminal row's chain order.
    let lco: String = sqlx::query_scalar(
        "select last_chain_order from inference_orders where orderbook_address=$1 and order_id=40",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lco, "co-1", "terminal row's last_chain_order must NOT be bumped by a late Filled");
    // The live counter-party (order 41, SELL) DID fill — that is correct, not part of the guard.
    assert_eq!(status_rem(&pool, ob, 41).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn filled_links_deal_to_orderbook_seller_buyer() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_deal_link_ob";
    let tc = "0:tc_deal_link";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // SELL leg (is_buy=false) by the seller note; order_id 1.
    let sell = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"1","isBuy":false,"price":"100","ticks":"10","note":"0:seller","tokenContract":tc,"deadline":"0","flags":"0"}),
    );
    project(&mut tx, &sell, &node(ob, "co-1")).await;
    // BUY leg by the buyer note; order_id 2.
    let buy = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"2","isBuy":true,"price":"100","ticks":"10","note":"0:buyer","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
    );
    project(&mut tx, &buy, &node(ob, "co-2")).await;
    // Filled crossing them; carries sellerTC + buyerNote.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer","sellerNote":"0:seller"}),
    );
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    assert_eq!(
        project(&mut tx, &filled, &node(ob, "co-deallink-3")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(orderbook.as_deref(), Some(ob));
    assert_eq!(seller.as_deref(), Some("0:seller"));
    assert_eq!(buyer.as_deref(), Some("0:buyer"));
}

#[tokio::test]
async fn orphan_repair_filled_links_deal() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_link_ob";
    let tc = "0:tc_orphan_link";
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Only the SELL leg present (the counterparty BUY OrderPlaced was dropped).
    let sell = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"1","isBuy":false,"price":"100","ticks":"10","note":"0:seller","tokenContract":tc,"deadline":"0","flags":"0"}),
    );
    project_inference_event(&mut tx, &sell, &node(ob, "co-1")).await.unwrap();
    // Expired Filled orphan: maker(1) present, taker(2) dropped.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer","sellerNote":"0:seller"}),
    );
    // Applied via the orphan path mints a global-PK inference_trades row — chain order
    // unique repo-wide.
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-orphanlink-2")).await.unwrap();
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(orderbook.as_deref(), Some(ob));
    assert_eq!(seller.as_deref(), Some("0:seller"), "seller resolved from present SELL leg");
    assert_eq!(buyer.as_deref(), Some("0:buyer"));

    // The present leg is the MAKER (the SELL), so resolve_is_buyer_maker takes the maker
    // branch: isBuyerMaker follows the maker's own side directly.
    let rows = tape_rows(&pool, ob).await;
    assert_eq!(
        rows.len(),
        1,
        "maker leg present resolves a direction; the match lands on the tape"
    );
    assert!(!rows[0].3, "maker leg is the SELL => taker bought");
}

#[tokio::test]
async fn orphan_repair_filled_no_leg_still_links() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_noleg_ob";
    let tc = "0:tc_orphan_noleg";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Neither leg present (both OrderPlaced dropped).
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer",
        // ZERO намеренно: тест про ФОЛБЭК — продавец обязан остаться невосстановимым,
        // когда SELL-нога не спроецирована, а событие его не назвало.
        "sellerNote":ZERO_ADDRESS}),
    );
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-1")).await.unwrap();
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(
        orderbook.as_deref(),
        Some(ob),
        "orderbook recorded from the event even with no legs"
    );
    assert_eq!(
        buyer.as_deref(),
        Some("0:buyer"),
        "buyer recorded from the event even with no legs"
    );
    assert!(seller.is_none(), "seller unresolved when the SELL leg was dropped");

    // Neither leg is present, so resolve_is_buyer_maker has no side to read from either
    // end of the match: the direction is unrecoverable and the row is omitted entirely,
    // rather than landing on the public tape with a guessed side.
    assert!(tape_rows(&pool, ob).await.is_empty(), "no tape row when neither leg is present");
}

#[tokio::test]
async fn orphan_repair_filled_taker_only_resolves_from_taker_side() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_takeronly_ob";
    let tc = "0:tc_orphan_takeronly";
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Only the BUY taker leg present (the counterparty SELL maker's OrderPlaced was
    // dropped) — the mirror of orphan_repair_filled_links_deal, which leaves the MAKER
    // leg present instead. With no maker row to read is_buy from, resolve_is_buyer_maker
    // falls back to inverting the taker's own side.
    let buy = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"2","isBuy":true,"price":"100","ticks":"10","note":"0:buyer","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
    );
    project_inference_event(&mut tx, &buy, &node(ob, "co-1")).await.unwrap();
    // Expired Filled orphan: taker(2) present, maker(1) dropped.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer",
        // ZERO намеренно: тест упражняет разрешение НАПРАВЛЕНИЯ по присутствующей ноге,
        // а не связку продавца — событие его не называет.
        "sellerNote":ZERO_ADDRESS}),
    );
    // Applied via the orphan path mints a global-PK inference_trades row — chain order
    // unique repo-wide.
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-orphantaker-2")).await.unwrap();
    tx.commit().await.unwrap();

    let rows = tape_rows(&pool, ob).await;
    assert_eq!(
        rows.len(),
        1,
        "taker leg present still resolves a direction; the match lands on the tape"
    );
    assert!(!rows[0].3, "taker is the BUY => isBuyerMaker is the inverse, false");
}

// ---- token_contract / deadline persistence ----

// Zero address the ABI decodes for an unset `address` field (64 zero hex digits after
// the workchain prefix). `inference_projectors::ZERO_ADDRESS` is `pub(crate)` and not
// importable from this integration-test crate, so it is declared again here.
const ZERO_ADDRESS: &str = "0:0000000000000000000000000000000000000000000000000000000000000000";

#[allow(clippy::too_many_arguments)]
async fn project_placed(
    pool: &PgPool,
    ob: &str,
    id: i64,
    is_buy: bool,
    price: &str,
    ticks: &str,
    tc: Option<&str>,
    deadline: i64,
) {
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": id.to_string(), "isBuy": is_buy, "price": price, "ticks": ticks,
            "note": "0:n",
            "tokenContract": tc.unwrap_or(ZERO_ADDRESS),
            "deadline": deadline.to_string(),"flags":"0",
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-placed-{id}");
    assert_eq!(project(&mut tx, &e, &node(ob, &co)).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
}

// Подписка приезжает обычным `InferenceOrderPlaced` с битом FLAG_SUBSCRIPTION (0x40):
// отдельного события в ABI книги нет. Предпосылка вызывающего теста — «строка рождается
// без дедлайна» — сохраняется: подписочный бид кладут с нулевым дедлайном и без TC.
async fn project_subscription(pool: &PgPool, ob: &str, id: i64, price: &str, ticks: &str) {
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": id.to_string(), "isBuy": true, "price": price, "ticks": ticks,
            "note": "0:bn", "tokenContract": ZERO_ADDRESS, "deadline": "0", "flags": "64",
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-sub-{id}");
    assert_eq!(project(&mut tx, &e, &node(ob, &co)).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
}

async fn project_placed_raw(
    pool: &PgPool,
    ob: &str,
    id: i64,
    value: serde_json::Value,
) -> anyhow::Result<ProjectionOutcome> {
    let e = ev("InferenceOrderPlaced", value);
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-raw-{id}");
    let outcome = project_inference_event(&mut tx, &e, &node(ob, &co)).await?;
    tx.commit().await.unwrap();
    Ok(outcome)
}

#[tokio::test]
async fn order_placed_persists_token_contract_and_deadline() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-persist";
    clean(&pool, ob).await;

    project_placed(&pool, ob, 7, /* is_buy */ false, "10", "5", Some("0:deal-tc"), 1760003600)
        .await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=7",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tc.as_deref(), Some("0:deal-tc"));
    assert_eq!(dl.as_deref(), Some("1760003600"));
}

#[tokio::test]
async fn buy_placement_normalizes_zero_token_contract_and_deadline_to_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-zero";
    clean(&pool, ob).await;

    project_placed(&pool, ob, 8, /* is_buy */ true, "10", "5", Some(ZERO_ADDRESS), 0).await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=8",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(tc.is_none(), "zero address must normalize to NULL");
    assert!(dl.is_none(), "zero deadline must normalize to NULL");
}

#[tokio::test]
async fn a_placement_missing_token_contract_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. Inserting the row with a NULL
    // TokenContract would be unrecoverable once it reaches a terminal status.
    let err = project_placed_raw(
        &pool,
        ob,
        10,
        serde_json::json!({
            "orderId": "10", "isBuy": false, "price": "10", "ticks": "5", "note": "0:n",
            "deadline": "0",
        }),
    )
    .await
    .expect_err("a missing tokenContract must fail the projection");
    assert!(format!("{err:#}").contains("tokenContract"));

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "no row may be inserted from an undecodable placement");
}

#[tokio::test]
async fn a_placement_missing_note_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:note-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. Inserting the row with a NULL
    // note would hide it from every `note=X` listing forever, and nothing ever repairs
    // `note_address` after the fact.
    let err = project_placed_raw(
        &pool,
        ob,
        11,
        serde_json::json!({
            "orderId": "11", "isBuy": false, "price": "10", "ticks": "5",
            "tokenContract": ZERO_ADDRESS, "deadline": "0",
        }),
    )
    .await
    .expect_err("a missing note must fail the projection");
    assert!(format!("{err:#}").contains("note"));

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "no row may be inserted from an undecodable placement");
}

#[tokio::test]
async fn a_placement_missing_deadline_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:deadline-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. Inserting the row with a NULL
    // deadline would be unrecoverable once it reaches a terminal status.
    let err = project_placed_raw(
        &pool,
        ob,
        12,
        serde_json::json!({
            "orderId": "12", "isBuy": false, "price": "10", "ticks": "5", "note": "0:n",
            "tokenContract": ZERO_ADDRESS,
        }),
    )
    .await
    .expect_err("a missing deadline must fail the projection");
    assert!(format!("{err:#}").contains("deadline"));

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "no row may be inserted from an undecodable placement");
}

#[tokio::test]
async fn replay_preserves_repaired_token_contract_and_deadline() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-replay";
    clean(&pool, ob).await;

    // A subscription row is born without a deadline: the event carries none.
    project_subscription(&pool, ob, 9, "10", "5").await;
    // The reconciler repairs it from the chain getter.
    sqlx::query("update inference_orders set deadline = 1760009999, token_contract = '0:repaired' where orderbook_address=$1 and order_id=9")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Replaying the original event must not wipe the repaired values.
    project_subscription(&pool, ob, 9, "10", "5").await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=9",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tc.as_deref(), Some("0:repaired"));
    assert_eq!(dl.as_deref(), Some("1760009999"));
}

// ---- inference_trades tape ----

/// `InferenceFilled` fixture. Named `filled_ev` because the bare `filled` reads as one of
/// this file's `filled_*` test fns.
fn filled_ev(maker_id: &str, taker_id: &str, ticks: &str, clearing: &str) -> DecodedEvent {
    ev(
        "InferenceFilled",
        serde_json::json!({
            "makerId": maker_id, "takerId": taker_id, "ticks": ticks,
            "clearingPrice": clearing,
            "sellerTC": "0:s", "buyerNote": "0:b", "sellerNote": "0:sn",
        }),
    )
}

async fn tape_rows(pool: &sqlx::PgPool, ob: &str) -> Vec<(String, String, String, bool)> {
    sqlx::query_as(
        "select trade_id, price::text, qty::text, is_buyer_maker
           from inference_trades where orderbook_address = $1 order by trade_id",
    )
    .bind(ob)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn filled_writes_one_tape_row_keyed_by_chain_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_write";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    // Resting BUY (maker) crossed by an incoming SELL (taker) => buyer IS the maker.
    place(&pool, &mut tx, ob, "1", true, "10", "tapew-1").await;
    place(&pool, &mut tx, ob, "2", false, "4", "tapew-2").await;
    let outcome =
        project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "tapew-3")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let rows = tape_rows(&pool, ob).await;
    assert_eq!(rows.len(), 1, "one Filled = exactly one tape row");
    assert_eq!(rows[0].0, "tapew-3", "trade_id is the Filled event's chain order");
    assert_eq!(rows[0].1, "1000000000", "price is the event's clearingPrice, raw");
    assert_eq!(rows[0].2, "4");
    assert!(rows[0].3, "maker leg is the BUY => isBuyerMaker");

    clean(&pool, ob).await;
}

#[tokio::test]
async fn filled_tape_row_takes_maker_side_when_maker_sells() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_maker_sells";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", false, "10", "tapems-1").await;
    place(&pool, &mut tx, ob, "2", true, "3", "tapems-2").await;
    project(&mut tx, &filled_ev("1", "2", "3", "1000000000"), &node(ob, "tapems-3")).await;
    tx.commit().await.unwrap();

    let rows = tape_rows(&pool, ob).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].3, "maker leg is the SELL => taker bought");

    clean(&pool, ob).await;
}

#[tokio::test]
async fn filled_tape_replay_is_idempotent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_replay";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", true, "10", "taperp-1").await;
    place(&pool, &mut tx, ob, "2", false, "4", "taperp-2").await;
    project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "taperp-3")).await;
    project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "taperp-3")).await;
    tx.commit().await.unwrap();

    assert_eq!(tape_rows(&pool, ob).await.len(), 1, "replay must not duplicate the tape row");

    clean(&pool, ob).await;
}

#[tokio::test]
async fn deferred_filled_writes_no_tape_row() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_deferred";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    // Only the maker leg exists: the Filled defers with ZERO writes, tape included.
    place(&pool, &mut tx, ob, "1", true, "10", "tapedf-1").await;
    let outcome =
        project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "tapedf-3")).await;
    assert_eq!(outcome, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();

    assert!(tape_rows(&pool, ob).await.is_empty());

    clean(&pool, ob).await;
}

#[tokio::test]
async fn a_real_order_placed_body_projects_into_inference_orders() {
    // IX-OB-25. Every other projector test in this file builds `DecodedEvent` through
    // `ev(...)`, so the field layout was asserted by intention: the test and the
    // projector agreed with each other and neither consulted the chain. Here the event
    // comes out of the decoder applied to real bytes, so the mapping is checked against
    // what the book actually emits.
    let Some(pool) = setup().await else { return };
    let src = "0:proj_real_inference_book";
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(src)
        .execute(&pool)
        .await
        .expect("purge");

    let decoded = Decoder::new()
        .expect("decoder")
        .decode_event_body(INFERENCE_ORDER_PLACED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known");
    // The projector routes on the `event_type` SUFFIX, not on `event_name`
    // (`inference_projectors.rs:53-56`): the reprojection path rebuilds `DecodedEvent`
    // with an empty `event_name`, so an arm matching the name would send every live
    // captured row to the seed-only branch. The decoder sets `event_type`, so this
    // routes the same way a live row does.
    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderPlaced");

    let n = node(src, "5f80projreal0000000000000001");
    let mut tx = pool.begin().await.expect("begin");
    let outcome = project(&mut tx, &decoded, &n).await;
    tx.commit().await.expect("commit");
    assert!(
        matches!(outcome, ProjectionOutcome::Applied),
        "a placement must project, got {outcome:?}"
    );

    // Read back what the projector wrote. Columns cast to text so the numerics come
    // out in one predictable form and the assertions below compare strings, the same
    // way the read model serves them.
    let row: (
        String,
        bool,
        String,
        String,
        String,
        bool,
        String,
        Option<String>,
        Option<String>,
        String,
    ) = sqlx::query_as(
        "select order_id::text, is_buy, price::text, amount_initial::text,
                    amount_remaining::text, is_subscription, note_address,
                    token_contract, deadline::text, status
               from inference_orders where orderbook_address = $1",
    )
    .bind(src)
    .fetch_one(&pool)
    .await
    .expect("the projected order row");

    // Expected values below are taken from the harvest journal's `decoded` snapshot
    // for this exact body (`fixtures::chain_bodies::HARVESTED_ORDER_PLACED_DECODED`):
    // {"note":"0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea",
    //  "flags":"0","isBuy":false,
    //  "price":"0x00000000000000000000000000000000000000000000000000000000b2d05e00",
    //  "ticks":"4","orderId":"534","deadline":"1786648380",
    //  "tokenContract":"0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"}
    // — NOT pasted from the first green run, which would only prove the projector
    // agrees with itself.
    assert_eq!(row.0, "534", "order_id <- orderId");
    assert_eq!(row.1, false, "is_buy <- isBuy");
    // price <- price, a uint256 hex in the payload (uint256_maybe_hex converts it to
    // decimal): 0x...b2d05e00 = 3_000_000_000.
    assert_eq!(row.2, "3000000000", "price <- price (hex uint256, decoded to decimal)");
    // amount_initial / amount_remaining <- ticks, both from the same payload field.
    assert_eq!(row.3, "4", "amount_initial <- ticks");
    assert_eq!(row.4, "4", "amount_remaining <- ticks");
    // is_subscription <- flags & FLAG_SUBSCRIPTION (0x40) != 0. The snapshot's flags
    // is "0", so the bit is clear and the row is not a subscription.
    assert_eq!(row.5, false, "is_subscription <- flags(0) & 0x40 != 0");
    assert_eq!(
        row.6, "0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea",
        "note_address <- note"
    );
    // token_contract <- tokenContract, through non_zero_address. The snapshot's
    // tokenContract is a real (non-zero) address, so it must survive as Some, not
    // collapse to NULL.
    assert_eq!(
        row.7.as_deref(),
        Some("0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"),
        "token_contract <- tokenContract (non-zero in the snapshot, must not collapse to NULL)"
    );
    // deadline <- deadline, through non_zero_uint. The snapshot's deadline is
    // "1786648380", non-zero, so it must survive as Some, not collapse to NULL.
    assert_eq!(
        row.8.as_deref(),
        Some("1786648380"),
        "deadline <- deadline (non-zero in the snapshot)"
    );
    assert_eq!(row.9, "OPEN", "status <- constant 'OPEN' on placement");
}
