//! Opening and configuring connections.

use crate::StoreError;
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

/// How long a connection waits for another writer before giving up. With one database thread
/// this only matters for tools and tests that open a second connection.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens (creating if needed) and configures a connection. Does not migrate.
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    Ok(conn)
}

/// Settings every connection gets: WAL (persistent in the file, so readers never block the
/// writer), `synchronous = NORMAL` (safe with WAL), foreign keys on, and a busy timeout.
/// Must run outside a transaction.
pub fn configure(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Invalid {
            column: "journal_mode",
            message: format!("expected wal, got {mode}"),
        });
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}
