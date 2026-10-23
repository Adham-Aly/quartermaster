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

/// Opens `state.db` as the disk mirror (`mirror.rs`). Foreign keys are not checked here: the mirror
/// holds rows whose references may name rows it doesn't keep, and the working database is the one
/// that checks them. Deleted pages are zeroed, so forgotten rows don't survive in the file. Each
/// mirror write is synced (`synchronous = FULL`): a row naming a file on disk (a transcript's
/// session, a pasted image's use) must survive a power cut as surely as the file does.
pub fn open_disk(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    configure_file(&conn)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", false)?;
    conn.pragma_update(None, "secure_delete", true)?;
    Ok(conn)
}

/// Opens the working database: in memory, with foreign keys on and temporary tables in memory too.
pub fn open_memory() -> Result<Connection, StoreError> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

/// Settings every connection to a file gets: WAL (persistent in the file, so readers never block
/// the writer), `synchronous = NORMAL` (safe with WAL), a busy timeout, and foreign keys on.
/// Must run outside a transaction.
pub fn configure(conn: &Connection) -> Result<(), StoreError> {
    configure_file(conn)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

fn configure_file(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Invalid {
            column: "journal_mode",
            message: format!("expected wal, got {mode}"),
        });
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}
