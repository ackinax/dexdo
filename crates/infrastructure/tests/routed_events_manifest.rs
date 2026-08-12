// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Направление 4 гарда формы: «событие ABI -> арм».
//!
//! Перечень событий ВЫВОДИТСЯ из ABI; руками задаётся только СКОУП — какие
//! контракты индексер обязан обслуживать. Это обратная сторона того, что делал
//! `UNPERSISTED_DEX_EVENTS`: тот перечислял события, которые МЫ решили не
//! проецировать, и о неизвестном нам событии знать не мог по построению.

use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;
use std::time::Duration;

use dodex_infrastructure::config::IGNORABLE_EVENT_TYPES;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors::project_event;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Контракты, чьи ABI индексер обслуживает ЦЕЛИКОМ.
const FULLY_IN_SCOPE: &[(&str, &str)] = &[
    ("InferenceOrderBook", "contracts/airegistry/InferenceOrderBook.abi.json"),
    ("TokenContract", "contracts/airegistry/TokenContract.abi.json"),
    // RootModel загружен волной 0 ради разрешения коллизии `ContractDeployed` по
    // `dst`. Проецировать его события не нужно, но АРМ обязан быть у обоих: без
    // арма событие уходит в `Unknown`, помечается processed и теряется навсегда.
    ("RootModel", "contracts/airegistry/RootModel.abi.json"),
];

/// Точечный скоуп внутри чужих ABI: в `PrivateNote` и `OracleEventList` живёт и
/// prediction-контур, поэтому здесь имена, а не весь файл.
const PARTIALLY_IN_SCOPE: &[&str] = &[
    "PrivateNote.InferenceOrderPlacedConfirmed",
    "PrivateNote.InferenceFilledConfirmed",
    "PrivateNote.InferenceOrderRemoved",
    "PrivateNote.InferenceOrderRejectedMirror",
    "PrivateNote.InferenceDealClosed",
    "PrivateNote.DealCredited",
    "PrivateNote.BookCredited",
    // Range-связка: матрица держит её в скоупе (§1), и без явной строки она
    // выпала бы ровно так же, как выпали два `*Confirmed`.
    "OracleEventList.RangeEventAdded",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn abi_event_names(rel_path: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(repo_root().join(rel_path))
        .unwrap_or_else(|e| panic!("read {rel_path}: {e}"));
    let abi: serde_json::Value = serde_json::from_str(&raw).expect("abi json");
    let events = abi["events"].as_array().expect("abi.events array");
    assert!(!events.is_empty(), "{rel_path}: events пуст — ключ ABI изменился, гард ослеп");
    events.iter().map(|e| e["name"].as_str().expect("event name").to_string()).collect()
}

fn in_scope_event_types() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (kind, path) in FULLY_IN_SCOPE {
        for name in abi_event_names(path) {
            out.insert(format!("{kind}.{name}"));
        }
    }
    out.extend(PARTIALLY_IN_SCOPE.iter().map(|s| s.to_string()));
    out
}

/// Узел-зонд. Три поля обязательны, и каждое по своей причине:
/// `src` — оба под-роутера начинают с посева скелета и без него падают ДО `match`;
/// `msg_chain_order` — `node_chain_order` возвращает `Err` без него;
/// `created_at` — проще подать время, чем разбирать, каким проекторам оно нужно.
fn probe_node() -> EventNode {
    EventNode {
        msg_id: "manifest_probe".to_string(),
        msg_chain_order: Some("00probe-1".to_string()),
        src: Some("0:manifest_probe".to_string()),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: Some(serde_json::json!(1_700_000_000)),
    }
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

#[test]
fn the_in_scope_set_is_thirty_four_events() {
    let n = in_scope_event_types().len();
    assert_eq!(
        n, 34,
        "скоуп изменился ({n} вместо 34). Это НЕ повод поправить число: сперва решить, \
         нужен ли новому событию проектор, и только потом менять и число, и арм"
    );
}

#[test]
fn no_in_scope_event_may_be_dropped_at_ingest() {
    // ГАРД, зелёный с первого прогона. `ignored_event_types` роняет edge ДО записи
    // в `raw_events`, то есть вне границы восстановления: ни реплей, ни
    // reprojection такое событие не вернут. Для inference-скоупа это навсегда
    // неверная read-модель, и решение такого веса не должно приниматься правкой
    // одной строки конфига.
    let scope = in_scope_event_types();
    for t in IGNORABLE_EVENT_TYPES {
        assert!(
            !scope.contains(t),
            "{t} входит в inference-скоуп и одновременно разрешён к отбрасыванию на приёме. \
             Отброшенный edge не попадает в raw_events, значит не восстанавливается ничем"
        );
    }
}

#[tokio::test]
async fn every_in_scope_abi_event_reaches_an_arm() {
    // ГАРД, зелёный с первого прогона: волна 0 раздала арм'ы всем 34 событиям.
    // Краснеет от НОВОГО события в ABI, у которого арма нет.
    //
    // `expect`, а не `else { return }`: этот тест — центральный в волне, и его
    // молчаливый пропуск без Postgres неотличим от прохождения.
    let pool = setup().await.expect(
        "гард маршрутизации требует Postgres: TEST_DATABASE_URL не задан или база не поднята",
    );

    // Флаги вместо счётчика: инвариант — «зонд доносит события ОБОИХ проецируемых
    // семейств до их армов», и выражать его числом нельзя — порог дрейфует вместе
    // со скоупом.
    let mut book_reached = false;
    let mut settlement_reached = false;

    for event_type in in_scope_event_types() {
        let mut tx = pool.begin().await.unwrap();
        let e = DecodedEvent {
            contract_kind: "",
            event_name: String::new(),
            event_type: event_type.clone(),
            // Пустой payload намеренно: цель — дойти до АРМА, а не спроецировать.
            value: serde_json::json!({}),
        };
        let outcome = project_event(&mut tx, &e, &probe_node()).await;

        // Для no-op-типов арма мало: он обязан быть ещё и пустым, иначе «no-op»
        // превращается в тихую запись. `txid_current_if_assigned()` возвращает
        // NULL ровно тогда, когда транзакция не писала ничего.
        let is_declared_no_op =
            event_type.starts_with("PrivateNote.") || event_type.starts_with("RootModel.");
        let write_xid: Option<i64> = if is_declared_no_op {
            sqlx::query_scalar("select txid_current_if_assigned()")
                .fetch_one(&mut *tx)
                .await
                .unwrap()
        } else {
            None
        };
        let _ = tx.rollback().await;

        // `Err` ДОПУСКАЕТСЯ и означает, что арм есть: проектор дошёл до разбора
        // полей и не нашёл их в пустом payload'е. Недопустим ровно `Ok(Unknown)` —
        // «никто не знает такого типа», то есть строка будет помечена processed и
        // потеряна навсегда.
        assert!(
            !matches!(outcome, Ok(ProjectionOutcome::Unknown)),
            "{event_type} есть в ABI скоупа, но не маршрутизируется: уйдёт в Unknown, \
             будет помечен processed на первом появлении и никогда не переспросится"
        );

        if is_declared_no_op {
            assert_eq!(
                outcome.expect("no-op арм не имеет права возвращать ошибку"),
                ProjectionOutcome::Applied,
                "{event_type} обязан быть no-op'ом, возвращающим Applied"
            );
            assert!(write_xid.is_none(), "{event_type} объявлен no-op'ом, но что-то записал");
        } else if outcome.is_ok() {
            if event_type.starts_with("InferenceOrderBook.") {
                book_reached = true;
            }
            if event_type.starts_with("TokenContract.") {
                settlement_reached = true;
            }
        }
    }

    // СТРАХОВКА ОТ ПУСТОГО ГАРДА. Ассерт выше отвергает только `Ok(Unknown)`,
    // поэтому `Err` на КАЖДОМ событии сделал бы его зелёным и бессодержательным.
    // Ровно это и происходит, если зонд отдать без `src`: оба под-роутера
    // начинают с посева скелета, который требует `node.src` и падает ДО `match`.
    assert!(
        book_reached,
        "ни одно InferenceOrderBook.* не вернуло Ok — зонд не доносит их до армов \
         (скорее всего у него нет `src`), и для всех девяти событий книги гард пуст"
    );
    assert!(
        settlement_reached,
        "ни одно TokenContract.* не вернуло Ok — то же для пятнадцати событий сеттлемента"
    );
}
