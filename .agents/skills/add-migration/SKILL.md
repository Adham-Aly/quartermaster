---
name: add-migration
description: Change the SQLite schema in qm-store - a new table, column, index or trigger, or a CHECK that must accept a new enum value - with a new migration file, its MIGRATIONS line, and the query module that uses it. Use when any code needs data the schema doesn't hold, or when adding a variant to a domain enum that a table column CHECKs.
---

# Adding a database migration

`state.db` is migrated at startup by `qm_store::schema::migrate`, all pending migrations in one
transaction (`rusqlite_migration`; a failure rolls everything back and the app refuses to start).
Progress is `PRAGMA user_version`, which counts entries in `MIGRATIONS`.

## Rules

- **Never edit, reorder or remove an applied migration**, and never insert one before the last:
  `user_version` counts positions, so users' databases would skip or repeat work. Only append.
- File names are `NNNN_<what>.sql` in `crates/qm-store/migrations/`. Numbers so far: 0001, 0002,
  0004, 0005, 0006, 0007 (0003 was never used); the next is **0008**.
- Conventions every table follows (`0001_initial.sql`): `STRICT`; `INTEGER PRIMARY KEY` ids (display
  IDs such as `T-0042` are these ids); instants as `INTEGER` milliseconds since the epoch, UTC, in
  `*_at` columns (`qm_store::time`); booleans `INTEGER` with `CHECK (x IN (0, 1))`; enums as their
  wire names with a `CHECK` listing them; JSON columns with `CHECK (json_valid(x))`.
- Nothing is deleted: retire rows with an `*_at` column. Append-only tables (`events`,
  `session_lines`) have a trigger refusing `UPDATE`; keep it that way.
- Every new table has a `seq` column (`INTEGER NOT NULL`, drawn from `qm_store::seq`; ordering uses
  it, never the id) and exactly one entry in `crates/qm-store/src/kept.rs`: KEPT with a predicate
  for the rows that reach `state.db`, or VOLATILE if none do. A test fails for a table missing from
  both or listed in both.
- Let the schema hold the invariants it can (`CHECK`, `UNIQUE`, partial unique indexes, foreign
  keys) as a backstop to the code.

## Steps

1. Write `crates/qm-store/migrations/0007_<what>.sql`. Start with a comment saying what it adds
   and why. `ALTER TABLE … ADD COLUMN` needs a default (or must be nullable) because rows exist.
2. Append one line to `MIGRATIONS` in `crates/qm-store/src/schema.rs`:
   `M::up(include_str!("../migrations/0007_<what>.sql")).foreign_key_check(),`
3. Put the queries in the table's module in `crates/qm-store/src/` (a new module gets a `mod`
   line in `lib.rs`). Queries are plain functions over `&Connection` / `&mut Connection`, called
   inside `Db::call` by the core; nothing else opens the database.
4. Map new enum columns with the `rusqlite` feature of `qm-types` (`ToSql`/`FromSql` on every ID
   and enum), so the stored text is always the wire name.

## A new value in a CHECKed enum

Adding a variant to a `qm_types::domain` enum that a column `CHECK`s (for example `Provider` in
`accounts.provider`, or `TaskStatus`) needs a migration, because SQLite can't alter a `CHECK`.
`WakeKind` and `HeldKind` are the exceptions: their columns have no closed `CHECK`, so a new kind
needs no migration. To rebuild a table:

```sql
PRAGMA defer_foreign_keys = ON;            -- foreign_keys can't be switched off inside the transaction
CREATE TABLE accounts_new ( … the same columns, the new CHECK … ) STRICT;
INSERT INTO accounts_new SELECT * FROM accounts;
DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;
-- recreate every index and trigger the old table had (find them in earlier migrations)
```

Copy the column list exactly from the earlier migrations, including columns later migrations
added. Run the migration against a copy of a real, populated `state.db` before you rely on it:
`foreign_key_check()` reports any broken reference and rolls back.

## Verify

`cargo xtask check`, `cargo build`, then start the app twice on a throwaway data directory and once
on a copy of an older one (the log says `database ready`), and read the new rows with
`sqlite3 -readonly` (skill `debug-from-logs`).
