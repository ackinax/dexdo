-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- Причина отказа реконсайлера жила только в логах: `stamp_failure` принимала один
-- аргумент — адрес книги, — а таблица держала лишь отметку времени и счётчик
-- попыток. Для оператора это означало поход в логи пода за ответом на вопрос
-- «почему эта книга не видна», а для DB-tail проверок — что «failing с причиной»
-- недоказуемо в принципе: у них нет доступа к логам.
--
-- NULL здесь законен и означает «отказов не было»; он же остаётся у строк,
-- отказавших до этой миграции.

alter table inference_markets add column last_reconcile_error text;

comment on column inference_markets.last_reconcile_error is
    'Human-readable reason for the most recent reconcile failure, written together '
    'with last_reconcile_failed_at. NULL when the book has never failed. Not cleared '
    'on success: last_reconcile_failed_at is the authority on whether the failure is '
    'current, and keeping the text lets an operator see what the last failure was. '
    'A benign NoBoc outcome also stamps a failure and lands its own fixed text here.';
