//! The schema, as migrations.
//!
//! [`MIGRATIONS`] is append-only: a new table or column is a new file `migrations/NNNN_<what>.sql`
//! and one line here, never an edit to an applied migration.
//! `rusqlite_migration` records progress in `PRAGMA user_version`; applying is idempotent.

use crate::StoreError;
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

/// The SQL is shared by the checked working-database migrations and the incomplete disk mirror.
const SQL: &[&str] = &[
    include_str!("../migrations/0001_initial.sql"),
    include_str!("../migrations/0002_crew.sql"),
    include_str!("../migrations/0004_delivery.sql"),
    include_str!("../migrations/0005_verification.sql"),
    include_str!("../migrations/0006_sessions.sql"),
    include_str!("../migrations/0007_tier_entries.sql"),
    include_str!("../migrations/0008_folder.sql"),
    include_str!("../migrations/0009_creation_order.sql"),
    include_str!("../migrations/0010_parent_cursor.sql"),
    include_str!("../migrations/0011_supervisor_payload.sql"),
    include_str!("../migrations/0012_forgotten_parent_facts.sql"),
    include_str!("../migrations/0013_autonomy_modes.sql"),
    include_str!("../migrations/0014_release_launches.sql"),
    include_str!("../migrations/0015_attachment_uses.sql"),
    include_str!("../migrations/0016_chat_kept_from.sql"),
    include_str!("../migrations/0017_in_place.sql"),
    include_str!("../migrations/0018_user_message_sources.sql"),
    include_str!("../migrations/0019_deferred_starts.sql"),
    include_str!("../migrations/0020_task_labels.sql"),
    include_str!("../migrations/0021_account_quota.sql"),
    include_str!("../migrations/0022_pending_move.sql"),
    include_str!("../migrations/0023_report_read.sql"),
    include_str!("../migrations/0024_user_words_cuts.sql"),
    include_str!("../migrations/0025_task_names.sql"),
];

/// Every migration, in order.
pub static MIGRATIONS: &[M<'static>] = &[
    M::up(SQL[0]).foreign_key_check(),
    M::up(SQL[1]).foreign_key_check(),
    M::up(SQL[2]).foreign_key_check(),
    M::up(SQL[3]).foreign_key_check(),
    M::up(SQL[4]).foreign_key_check(),
    M::up(SQL[5]).foreign_key_check(),
    M::up(SQL[6]).foreign_key_check(),
    M::up(SQL[7]).foreign_key_check(),
    M::up(SQL[8]).foreign_key_check(),
    M::up(SQL[9]).foreign_key_check(),
    M::up(SQL[10]).foreign_key_check(),
    M::up(SQL[11]).foreign_key_check(),
    M::up(SQL[12]).foreign_key_check(),
    M::up(SQL[13]).foreign_key_check(),
    M::up(SQL[14]).foreign_key_check(),
    M::up(SQL[15]).foreign_key_check(),
    M::up(SQL[16]).foreign_key_check(),
    M::up(SQL[17]).foreign_key_check(),
    M::up(SQL[18]).foreign_key_check(),
    M::up(SQL[19]).foreign_key_check(),
    M::up(SQL[20]).foreign_key_check(),
    M::up(SQL[21]).foreign_key_check(),
    M::up(SQL[22]).foreign_key_check(),
    M::up(SQL[23]).foreign_key_check(),
];

/// Upgrades an intentionally incomplete mirror. Its foreign keys are repaired and validated only
/// after kept rows have been loaded into the working database (`mirror::load`). Checking them
/// during a disk migration would reject references to deliberately unmirrored conversation rows.
pub(crate) fn migrate_mirror(conn: &mut Connection) -> Result<(), StoreError> {
    Migrations::new(SQL.iter().map(|sql| M::up(sql)).collect()).to_latest(conn)?;
    Ok(())
}

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
