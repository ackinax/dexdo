-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- The reconciler's failure reason lived only in the logs: `stamp_failure` took a
-- single argument — the book's address — while the table held just a timestamp and an
-- attempt counter. For an operator that meant digging through pod logs to answer "why
-- is this book not visible", and for DB-tail checks it meant "failing with a reason"
-- was unprovable in principle: they have no access to logs.
--
-- NULL is legitimate here and means "there were no failures"; it also remains on rows
-- that failed before this migration.

alter table inference_markets add column last_reconcile_error text;

comment on column inference_markets.last_reconcile_error is
    'Human-readable reason for the most recent reconcile failure, written together '
    'with last_reconcile_failed_at. NULL when the book has never failed. Not cleared '
    'on success: last_reconcile_failed_at is the authority on whether the failure is '
    'current, and keeping the text lets an operator see what the last failure was. '
    'A benign NoBoc outcome also stamps a failure and lands its own fixed text here.';
