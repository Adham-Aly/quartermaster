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
