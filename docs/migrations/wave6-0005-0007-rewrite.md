# Migrations 0005–0007 were edited in place (inference indexer waves)

**Who this affects:** any database that applied migrations `0005`, `0006` or
`0007` *before* this change — that is, any long-lived test database or stand
that ran the `feature/inference-indexer-wave0` … `wave6` branches. Production
and `dev` are **not** affected: they carry only `0001`–`0004`.

**Symptom:** the next connection fails with `VersionMismatch(5)` (or 6/7).
`sqlx::migrate!` records a checksum per applied file and refuses to run when a
recorded file's content has changed, so neither the indexer nor the DB-backed
tests will start until the schema is recreated.

**What changed:** only the `--` header prose of the three files (translated to
English, and `0007`'s rationale corrected — see below). No DDL was touched:
`0005` still issues the same `comment on column`, `0006` still adds
`last_reconcile_error`, `0007` still creates `raw_events_created_at_idx`. The
schema those files produce is byte-identical to what they produced before.

**Fix:** drop and recreate the schema, then let the migrations re-apply:

```sh
psql "$TEST_DATABASE_URL" -c 'drop schema public cascade; create schema public;'
```

For a deployed stand, follow the full procedure in
[`deploy/ansible/README.md`](../../deploy/ansible/README.md#migration-checksums-and-schema-recreation)
— in particular stop the stack **before** wiping, or the old indexer reapplies
its own copy of the file into the empty schema and the new one then fails the
same check.

**Why this was acceptable here:** the three files are unreleased. `dev` carries
`0001`–`0004`, so no production database has ever recorded their checksums, and
the only cost is recreating disposable databases. Editing an applied migration
that has reached `dev` or production is **not** acceptable — ship a new
migration instead.

## The corrected claim in 0007

The original header stated that `CONCURRENTLY` was unavailable "because
`sqlx::migrate!` runs the migration in a transaction". That is false: sqlx runs
a migration outside a transaction when the file opens with `-- no-transaction`
(`sqlx-core`'s `migrate::source`), which is exactly what `create index
concurrently` requires. The index is built blocking by choice — a failed
concurrent build leaves an `INVALID` index to find and drop by hand, a worse
failure mode than a slow deploy for an index this cheap. The same wrong claim
was propagated to `docs/tech-specs/data-schema.md` and has been corrected there.
