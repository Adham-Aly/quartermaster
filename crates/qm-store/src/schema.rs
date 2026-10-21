//! The schema, as migrations.
//!
//! [`MIGRATIONS`] is append-only: a new table or column is a new file `migrations/NNNN_<what>.sql`
//! and one line here, never an edit to an applied migration.
//! `rusqlite_migration` records progress in `PRAGMA user_version`; applying is idempotent.

use crate::StoreError;
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

/// Every migration, in order.
pub static MIGRATIONS: &[M<'static>] = &[
    M::up(include_str!("../migrations/0001_initial.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0002_crew.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0004_delivery.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0005_verification.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0006_sessions.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0007_tier_entries.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0008_folder.sql")).foreign_key_check(),
    M::up(include_str!("../migrations/0009_creation_order.sql")).foreign_key_check(),
];

/// The migration set.
pub fn migrations() -> Migrations<'static> {
    Migrations::from_slice(MIGRATIONS)
}

/// Brings a configured connection's database up to date. Already-applied migrations are skipped, so
/// re-running is a no-op. Startup runs this on the database thread.
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrations().to_latest(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database as it was before creation order: every table up to migration 0007, with an
    /// event that has an ID but no `seq`.
    fn before_creation_order() -> Connection {
        let mut conn = Connection::open_in_memory().expect("memory database");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        Migrations::from_slice(&MIGRATIONS[..5])
            .to_latest(&mut conn)
            .expect("the old schema");
        conn.execute(
            "INSERT INTO events (at, kind, summary, payload) VALUES (1, 'a', 'first', '{}'),
                                                                    (2, 'b', 'second', '{}')",
            [],
        )
        .expect("old events");
        conn
    }

    #[test]
    fn creation_order_backfills_seq_from_ids_and_keeps_the_append_only_triggers() {
        let mut conn = before_creation_order();
        migrate(&mut conn).expect("the upgrade");

        let pairs: Vec<(i64, i64)> = conn
            .prepare("SELECT id, seq FROM events ORDER BY id")
            .expect("query")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("values");
        assert_eq!(
            pairs,
            vec![(1, 1), (2, 2)],
            "an upgraded row keeps its ID as its seq"
        );

        let update = conn.execute("UPDATE events SET summary = 'changed'", []);
        assert!(update.is_err(), "events stay append-only after the upgrade");
        let delete = conn.execute("DELETE FROM events", []);
        assert!(delete.is_err(), "events stay append-only after the upgrade");
    }
}
