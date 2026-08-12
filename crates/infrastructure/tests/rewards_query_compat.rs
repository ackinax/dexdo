// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Совместимость схемы с запросом dodex-points-rewards. Потребитель живёт в другом
// репозитории и его тесты гоняются отдельно, поэтому изменение колонок здесь
// ломает его молча — этот тест переводит поломку в красный на нашей стороне.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Дословно `resolve_deal` из
/// dodex-points-rewards/crates/infrastructure/src/indexer_reader.rs:52-57.
/// Пять выражений — ровно то, что потребитель читает. Ни `finalized_ticks`,
/// ни `close_kind` он не запрашивает, и требовать их здесь значило бы
/// придумывать обязательство, которого никто не брал.
const REWARDS_RESOLVE_DEAL: &str = "select orderbook_address, seller_note, buyer_note, clean_settlement, \
     (settled_at_chain is not null) as settled \
     from inference_deals where token_contract_address = $1";

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
async fn the_rewards_resolve_deal_query_still_decodes() {
    let Some(pool) = setup().await else { return };
    let tc = "0:rewards_compat_probe";
    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    // Строка обязательна: на пустой выборке `query_as` НИЧЕГО не декодирует, и
    // несовместимость типа колонки прошла бы незамеченной — проверялось бы лишь
    // наличие имён.
    sqlx::query(
        "insert into inference_deals \
         (token_contract_address, orderbook_address, seller_note, buyer_note, clean_settlement, settled_at_chain) \
         values ($1, '0:ob', '0:seller', '0:buyer', true, now())",
    )
    .bind(tc)
    .execute(&pool)
    .await
    .unwrap();

    let row: (Option<String>, Option<String>, Option<String>, Option<bool>, bool) =
        sqlx::query_as(REWARDS_RESOLVE_DEAL)
            .bind(tc)
            .fetch_one(&pool)
            .await
            .expect("запрос rewards обязан оставаться валидным против схемы dexdo");

    assert_eq!(row.0.as_deref(), Some("0:ob"));
    assert_eq!(row.1.as_deref(), Some("0:seller"));
    assert_eq!(row.2.as_deref(), Some("0:buyer"));
    assert_eq!(row.3, Some(true));
    assert!(row.4, "settled выводится из settled_at_chain");

    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
}
