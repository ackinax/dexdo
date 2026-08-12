// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Наблюдатель: сквозные инварианты индексера по ВСЕМУ прогону e2e.
//!
//! Трафика не создаёт — читает состояние, оставленное сценариями. Поэтому
//! запускается последним шагом пайплайна и со `status: [success, failure]`:
//! прогон, в котором сценарий упал, тем более нуждается в диагностике.
//!
//! Два свойства делают его пригодным как БЛОКИРУЮЩИЙ шаг, и без них он падал бы
//! по неправильной причине регулярно, а по правильной — почти никогда:
//!
//! * он ОПРАШИВАЕТ до сходимости с дедлайном, а не снимает срез. Захват тикает
//!   3 с, реконсайлер 15 с, видимость штампуется после полного цикла sweep'а;
//!   книга, посеянная за секунды до хвоста, законно `discovering`;
//! * каждое утверждение ограничено ОКНОМ прогона. Postgres стенда переживает
//!   пайплайны, и книга, брошенная отменённым прогоном, иначе роняла бы
//!   следующий по чужой причине.
//!
//! Своего SQL здесь нет: все предикаты живут в `IndexerRepository`, потому что
//! `WEDGED_BOOKS_WHERE` и `PENDING_PROJECTION_WHERE` — единственные источники
//! соответствующих гейджей, а IX-MET-03 требует совпадения проверки с гейджем.
//!
//! `#[ignore]` — как у остальных e2e-бинарей: локальный прогон их не трогает,
//! CI зовёт с `--run-ignored only`.

use std::env;
use std::time::Duration;
use std::time::Instant;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use sqlx::postgres::PgPoolOptions;

/// `None` означает «шаг запущен вне стенда»: утверждать нечего, тест печатает
/// причину и выходит. На хосте переменная приходит из секрета пайплайна, и её
/// отсутствие там ловится не здесь, а гардом пустой выборки в скрипте.
async fn observer_repo() -> Option<IndexerRepository> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("observer: TEST_DATABASE_URL not set — nothing to observe");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("observer: connect to TEST_DATABASE_URL");
    database::run_migrations(&pool).await.expect("observer: run migrations");
    Some(IndexerRepository::new(pool))
}

/// Начало прогона, unix-секунды. Скрипт на хосте вычисляет её как
/// `now_on_host - elapsed`, где `elapsed` снят целиком по часам CI-раннера, так
/// что смещение часов сокращается.
///
/// Отсутствие переменной — не молчаливое послабление: окно берётся суточным, и
/// об этом печатается. Сутки — не «почти всегда», а честная граница: на стенде с
/// persistent-базой без окна якорь удовлетворился бы позавчерашним прогоном.
fn run_window() -> i64 {
    match env::var("E2E_STARTED_AT").ok().and_then(|v| v.parse::<i64>().ok()) {
        Some(t) => t,
        None => {
            let fallback = chrono::Utc::now().timestamp() - 86_400;
            eprintln!(
                "observer: E2E_STARTED_AT not set — falling back to a 24h window. \
                 Assertions still hold, but residue from a run inside that window \
                 is indistinguishable from this run's own work"
            );
            fallback
        }
    }
}

/// Дедлайн сходимости. Значение живёт ЗДЕСЬ и больше нигде: скрипт на хосте
/// переменную только пробрасывает и своего дефолта не имеет. Второй источник той
/// же величины разошёлся бы тихо — скрипт продолжал бы честно печатать неправду
/// про худший случай после правки одной лишь rust-стороны.
const DEFAULT_DEADLINE_SECS: u64 = 240;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

fn deadline() -> Duration {
    let secs = env::var("OBSERVER_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DEADLINE_SECS);
    // Печатается каждым тестом, потому что дедлайн у каждого свой. Худший случай
    // шага — сумма: в бинаре два `#[ignore]`-теста, и скрипт зовёт их с
    // `--test-threads 1`. Бюджет пайплайна узкий, и это число обязано быть видно
    // в выводе, а не выводиться читателем из исходников.
    eprintln!(
        "observer: deadline {secs}s for this test; the binary has two, run with \
         --test-threads 1, so a non-converging step costs about {}s plus compilation",
        secs * 2
    );
    Duration::from_secs(secs)
}

/// Один снимок: `Ok(())` — всё сошлось, `Err(текст)` — что именно ещё не сошлось.
async fn snapshot(repo: &IndexerRepository, since: i64) -> anyhow::Result<Result<(), String>> {
    let mut off: Vec<String> = Vec::new();

    let pending = repo.pending_projection_since(since).await?;
    if !pending.is_empty() {
        let total: i64 = pending.iter().map(|(_, n)| *n).sum();
        off.push(format!(
            "backlog не сошёлся: {total} строк принято в окне и не спроецировано — {pending:?}"
        ));
    }

    let books = repo.inference_books_with_events_since(since).await?;
    let without = repo.inference_books_without_verdict(&books).await?;
    if !without.is_empty() {
        off.push(format!(
            "книги без вердикта (ни visible, ни superseded, ни failing С ПРИЧИНОЙ): {without:?}"
        ));
    }

    let wedged = repo.inference_wedged_book_addresses(&books).await?;
    if !wedged.is_empty() {
        off.push(format!("видимые книги держат необработанные события: {wedged:?}"));
    }

    Ok(if off.is_empty() { Ok(()) } else { Err(off.join("\n  ")) })
}

/// Печатается на ОБОИХ исходах, и это не симметрия ради симметрии: распределение
/// причин нужнее всего как раз на упавшем прогоне, а `panic!` стоит внутри цикла
/// и до хвоста теста не доходит. Без неё «причина названа» неотличимо от
/// «написано хоть что-то».
///
/// Ошибки запросов здесь глотаются в значения по умолчанию намеренно: на красном
/// пути диагностика не имеет права подменить собой настоящую причину падения.
async fn print_diagnostics(repo: &IndexerRepository, since: i64, elapsed: Duration) {
    let books = repo.inference_books_with_events_since(since).await.unwrap_or_default();
    let undecodable = repo.count_undecodable_since(since).await.unwrap_or(-1);
    let failing = repo.inference_failing_books(&books).await.unwrap_or_default();
    eprintln!(
        "observer: {}s; книг в окне {}; недекодируемых строк {undecodable} \
         (диагностика, не отказ; -1 значит, что сам запрос не удался); \
         failing с причиной: {failing:?}",
        elapsed.as_secs(),
        books.len()
    );
}

#[tokio::test]
#[ignore = "e2e: reads the stand database at the tail of a run"]
async fn the_run_converged_and_every_book_of_it_has_a_verdict() {
    let Some(repo) = observer_repo().await else { return };
    let since = run_window();
    let limit = deadline();

    let started = Instant::now();
    loop {
        match snapshot(&repo, since).await.expect("observer: snapshot query failed") {
            Ok(()) => break,
            Err(why) => {
                if started.elapsed() >= limit {
                    print_diagnostics(&repo, since, started.elapsed()).await;
                    panic!(
                        "observer: инварианты не сошлись за {}s:\n  {why}\n\
                         Дедлайн переопределяется OBSERVER_DEADLINE_SECS. Опрос, а не срез, \
                         потому что захват тикает 3s, реконсайлер 15s, а видимость \
                         штампуется после полного цикла sweep'а",
                        limit.as_secs()
                    );
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }

    print_diagnostics(&repo, since, started.elapsed()).await;
}

#[tokio::test]
#[ignore = "e2e: reads the stand database at the tail of a run"]
async fn at_least_one_visible_book_carries_an_order_and_events_from_this_run() {
    let Some(repo) = observer_repo().await else { return };
    let since = run_window();
    let limit = deadline();

    // Опрос по той же причине, что и у диагностики: книга последнего сценария
    // становится видимой только после цикла реконсайлера, и срез поймал бы её
    // в законном `discovering`.
    let started = Instant::now();
    loop {
        let anchored =
            repo.inference_anchored_books_since(since).await.expect("observer: anchor query failed");
        if !anchored.is_empty() {
            eprintln!("observer: якорь — {} видимых книг с ордерами: {anchored:?}", anchored.len());
            break;
        }
        if started.elapsed() >= limit {
            // Печать здесь нужнее, чем в диагностике. Текст ассерта ниже
            // покрывает ДВА разных диагноза: трафик до индексера не доехал
            // вовсе (ошибка `dapp_id` или `dst`-фильтра — ровно та дыра, ради
            // которой матрица разводит якорь с диагностикой) либо доехал, но
            // видимым ничего не стало (встал реконсайлер). Различает их
            // «книг в окне»: ноль против ненуля. Своей печати у соседнего теста
            // при `--test-threads 1` может и не случиться — он мог упасть раньше.
            print_diagnostics(&repo, since, started.elapsed()).await;
            panic!(
                "observer: за {}s не нашлось ни одной видимой книги со спроецированным \
                 ордером и событиями этого прогона. Смотреть на «книг в окне» в строке \
                 выше: ноль значит, что трафик до индексера не доехал; ненуль — что \
                 доехал, но видимым ничего не стало. Диагностический шаг такой прогон \
                 считает идеальным — пустая база проходит все его утверждения, — \
                 поэтому якорь и существует отдельно",
                limit.as_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
