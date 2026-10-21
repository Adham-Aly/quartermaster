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

    /// The schema just before the folder migration, which is the seventh entry in `MIGRATIONS`.
    fn before_folders() -> Connection {
        let mut conn = Connection::open_in_memory().expect("a memory database");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        migrations()
            .to_version(&mut conn, 6)
            .expect("the schema before folders");
        conn
    }

    fn insert_workspace(conn: &Connection, mode: &str) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO projects (name, path, origin, default_branch, delivery_mode, merge_method, \
             created_at) VALUES ('personal', '/work', 'workspace', 'main', ?1, 'squash', 0)",
            [mode],
        )
    }

    #[test]
    fn the_folder_migration_moves_the_workspace_row_to_folder() {
        let mut conn = before_folders();
        insert_workspace(&conn, "branch_only").expect("the old workspace row");
        migrate(&mut conn).expect("the folder migration");
        let mode: String = conn
            .query_row(
                "SELECT delivery_mode FROM projects WHERE origin = 'workspace'",
                [],
                |row| row.get(0),
            )
            .expect("the workspace row");
        assert_eq!(mode, "folder");
    }

    #[test]
    fn the_folder_mode_is_accepted_and_nonsense_is_not() {
        let mut conn = before_folders();
        migrate(&mut conn).expect("the folder migration");
        insert_workspace(&conn, "folder").expect("a folder project");
        assert!(insert_workspace(&conn, "works_in_folder").is_err());
    }
}
