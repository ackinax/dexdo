// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Окна прогона для наблюдателя e2e. Каждый запрос здесь ограничен либо
//! временем ПРИЁМА (`raw_events.created_at`), либо набором адресов, поэтому
//! тесты безопасны в общей БД и не зависят от чужих строк — в отличие от
//! глобальных агрегатов, на которых волна 1 потеряла время.

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

/// Строка `raw_events` с заданным возрастом приёма и заданной декодируемостью.
async fn seed_raw(pool: &PgPool, msg: &str, src: &str, event_type: Option<&str>, age_secs: i64) {
    sqlx::query(
        "insert into raw_events
           (msg_id, chain_order, created_at, created_at_chain, src_address, dst_address,
            event_type, body_json, decoded)
         values ($1, $1, now() - make_interval(secs => $4), now(), $2, null, $3, '{}'::jsonb,
                 case when $3::text is null then null else '{}'::jsonb end)
         on conflict (msg_id) do nothing",
    )
    .bind(msg)
    .bind(src)
    .bind(event_type)
    .bind(age_secs as f64)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_book(
    pool: &PgPool,
    ob: &str,
    reconciled: bool,
    failed: bool,
    reason: Option<&str>,
    superseded: bool,
) {
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into inference_markets
           (orderbook_address, created_at_chain, last_reconciled_at,
            last_reconcile_failed_at, last_reconcile_error, superseded_at)
         values ($1, now(),
                 case when $2 then now() else null end,
                 case when $3 then now() else null end,
                 $4,
                 case when $5 then now() else null end)",
    )
    .bind(ob)
    .bind(reconciled)
    .bind(failed)
    .bind(reason)
    .bind(superseded)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn the_pending_window_excludes_rows_ingested_before_the_run() {
    // Окно существует ровно потому, что база стенда переживает пайплайны:
    // без него строка, брошенная позавчерашним прогоном, роняет сегодняшний.
    let Some(pool) = setup().await else { return };
    let src = "0:obsq_pending";
    // Тип уникален для этого теста. Иначе соседний тест, пишущий строку того же
    // типа, сделал бы строгое равенство ниже флаки, а нестрогое сравнение не
    // отличило бы «окно сузило» от «окно проигнорировано».
    let ty = "InferenceOrderBook.ObsWindowProbe";
    let undec_new = "0:obsq_undec_new";
    let undec_old = "0:obsq_undec_old";
    let undec_scope: Vec<String> = [undec_new, undec_old].iter().map(|s| s.to_string()).collect();
    for a in [src, undec_new, undec_old] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
    }
    seed_raw(&pool, "obsq-old", src, Some(ty), 7200).await;
    seed_raw(&pool, "obsq-new", src, Some(ty), 5).await;
    seed_raw(&pool, "obsq-undec-old", undec_old, None, 7200).await;
    seed_raw(&pool, "obsq-undec-new", undec_new, None, 5).await;

    let repo = IndexerRepository::new(pool.clone());
    let now = chrono::Utc::now().timestamp();
    let count_of =
        |rows: &[(String, i64)]| -> i64 { rows.iter().filter(|(t, _)| t == ty).map(|(_, n)| *n).sum() };

    let narrow = repo.pending_projection_since(now - 60).await.unwrap();
    assert_eq!(count_of(&narrow), 1, "в узкое окно обязана попасть ровно свежая строка");
    let wide = repo.pending_projection_since(now - 86_400).await.unwrap();
    assert_eq!(count_of(&wide), 2, "в широкое — обе: значит окно сужает, а не игнорируется");

    // Недекодируемые считаются ОТДЕЛЬНО от проецируемых и тем же окном.
    //
    // Проверка идёт через СКОУПНЫЙ вариант, а не через разность двух глобальных
    // счётчиков: `count_undecodable_since` — единственный метод здесь без скоупа,
    // и сравнение двух его чтений ломается от постороннего писателя.
    // Конкурент известен поимённо — `capture.rs`
    // (`persist_page_handles_mixed_decodable_and_undecodable_edges`) вставляет
    // недекодируемую строку и затем её вычищает; удаление между двумя чтениями
    // ломает неравенство при полностью исправном окне.
    let mut in_narrow = repo.undecodable_addresses_since(now - 60, &undec_scope).await.unwrap();
    in_narrow.sort();
    assert_eq!(
        in_narrow,
        vec![undec_new.to_string()],
        "в узкое окно попадает только свежая недекодируемая строка"
    );
    let mut in_wide = repo.undecodable_addresses_since(now - 86_400, &undec_scope).await.unwrap();
    in_wide.sort();
    assert_eq!(in_wide, vec![undec_new.to_string(), undec_old.to_string()], "в широкое — обе");
    // Глобальный счётчик — тот, что зовёт наблюдатель. Утверждение односторонее и
    // от чужих строк не зависит: наша свежая строка в окне есть и никуда не денется.
    assert!(
        repo.count_undecodable_since(now - 60).await.unwrap() >= 1,
        "свежая недекодируемая строка обязана считаться и глобальным методом"
    );

    for a in [src, undec_new, undec_old] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_book_scope_holds_only_books_with_events_in_this_run() {
    // Этот метод решает, КОГО наблюдатель вообще судит: и проверка вердикта, и
    // wedged берут скоуп у него. Неверное окно здесь оставило бы соседние тесты
    // зелёными, а на стенде дало бы либо суд над всеми книгами, что когда-либо
    // существовали, либо ни над одной. Общая константа `EVENTS_IN_WINDOW`
    // гарантирует, что предикат у двух методов ОДИНАКОВ, — но не что множество
    // правильное, и «книга позавчерашнего прогона не роняет сегодняшний»
    // держалось бы на непроверенном методе.
    let Some(pool) = setup().await else { return };
    let fresh = "0:obsq_scope_fresh";
    let stale = "0:obsq_scope_stale";
    let silent = "0:obsq_scope_silent";
    let all: Vec<String> = [fresh, stale, silent].iter().map(|s| s.to_string()).collect();
    for ob in [fresh, stale, silent] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(ob)
            .execute(&pool)
            .await
            .unwrap();
        seed_book(&pool, ob, true, false, None, false).await;
    }
    seed_raw(&pool, "obsq-scope-stale", stale, Some("InferenceOrderBook.InferenceFilled"), 7200)
        .await;

    let repo = IndexerRepository::new(pool.clone());
    let now = chrono::Utc::now().timestamp();

    let narrow = repo.inference_books_with_events_since(now - 60).await.unwrap();
    assert!(!narrow.contains(&fresh.to_string()), "книга без событий в окне в скоуп не входит");
    assert!(
        !narrow.contains(&stale.to_string()),
        "книга, чьё единственное событие СТАРШЕ окна, в скоуп не входит — иначе \
         брошенная отменённым прогоном роняла бы следующий по чужой причине"
    );

    seed_raw(&pool, "obsq-scope-fresh", fresh, Some("InferenceOrderBook.InferenceFilled"), 5).await;
    // Строка ОБРАБОТАНА: на хвосте прогона таких большинство, и скоуп обязан их
    // видеть. Это же и причина, по которой индекс 0007 не может быть частичным
    // по `processed_at is null`, как оба существующих индекса по `src_address`.
    sqlx::query("update raw_events set processed_at = now() where msg_id = 'obsq-scope-fresh'")
        .execute(&pool)
        .await
        .unwrap();

    let narrow = repo.inference_books_with_events_since(now - 60).await.unwrap();
    assert!(
        narrow.contains(&fresh.to_string()),
        "книга с событием в окне обязана попасть в скоуп, в том числе с обработанным"
    );
    assert!(!narrow.contains(&silent.to_string()), "книга без событий вообще в скоуп не входит");

    let wide = repo.inference_books_with_events_since(now - 86_400).await.unwrap();
    assert!(
        wide.contains(&stale.to_string()),
        "в широком окне старая книга появляется — значит выбор делает именно окно"
    );

    for ob in [fresh, stale, silent] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(ob)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&all)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_verdict_needs_a_reason_and_a_superseded_book_needs_none() {
    let Some(pool) = setup().await else { return };
    let visible = "0:obsq_visible";
    let failing_with = "0:obsq_failing_with";
    let failing_without = "0:obsq_failing_without";
    let superseded = "0:obsq_superseded";
    let discovering = "0:obsq_discovering";
    let scope: Vec<String> = [visible, failing_with, failing_without, superseded, discovering]
        .iter()
        .map(|s| s.to_string())
        .collect();

    seed_book(&pool, visible, true, false, None, false).await;
    seed_book(&pool, failing_with, false, true, Some("getVersion reverted"), false).await;
    seed_book(&pool, failing_without, false, true, None, false).await;
    seed_book(&pool, superseded, false, false, None, true).await;
    seed_book(&pool, discovering, false, false, None, false).await;

    let repo = IndexerRepository::new(pool.clone());
    let mut without = repo.inference_books_without_verdict(&scope).await.unwrap();
    without.sort();
    assert_eq!(
        without,
        vec![discovering.to_string(), failing_without.to_string()],
        "вердикта нет ровно у ещё не разобранной книги и у той, чей отказ БЕЗ ТЕКСТА; \
         superseded — полноценный третий вердикт, а не смягчение"
    );

    let failing = repo.inference_failing_books(&scope).await.unwrap();
    assert_eq!(
        failing,
        vec![(failing_with.to_string(), "getVersion reverted".to_string())],
        "печатаемый список failing несёт причину: без неё шаг зелёный, но нечитаемый"
    );

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&scope)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_anchor_finds_a_visible_book_with_orders_and_events_in_the_window() {
    let Some(pool) = setup().await else { return };
    let ob = "0:obsq_anchor";
    seed_book(&pool, ob, true, false, None, false).await;
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let repo = IndexerRepository::new(pool.clone());
    let window = chrono::Utc::now().timestamp() - 60;

    // Ни ордеров, ни событий — якорь пуст.
    assert!(repo.inference_anchored_books_since(window).await.unwrap().iter().all(|a| a != ob));

    sqlx::query(
        "insert into inference_orders
           (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
            status, last_chain_order, chain_created_at, chain_updated_at)
         values ($1, 1, true, 1, 10, 10, 'OPEN', '00obsq', now(), now())",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    // Ордер есть, а события прогона — нет: якорь всё ещё пуст.
    assert!(repo.inference_anchored_books_since(window).await.unwrap().iter().all(|a| a != ob));

    seed_raw(&pool, "obsq-anchor-ev", ob, Some("InferenceOrderBook.InferenceOrderPlaced"), 5).await;
    assert!(repo.inference_anchored_books_since(window).await.unwrap().iter().any(|a| a == ob));

    // И вне окна — снова пусто: окно действительно сужает, а не украшает.
    let future = chrono::Utc::now().timestamp() + 3600;
    assert!(repo.inference_anchored_books_since(future).await.unwrap().iter().all(|a| a != ob));

    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}
