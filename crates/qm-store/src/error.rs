//! What can go wrong in the store.

/// A store error. Messages never carry a secret (the store holds none: credentials live in
/// `auth.json`, and `accounts.credential_ref` is only a key).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("{column} holds invalid JSON: {source}")]
    Json {
        column: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("{column} holds an invalid value: {message}")]
    Invalid {
        column: &'static str,
        message: String,
    },
    #[error("the database thread has stopped")]
    ThreadStopped,
    #[error("a database call panicked: {0}")]
    Panicked(String),
    #[error("new {table} numbers can't be issued until the app restarts: {reason}")]
    NumbersHeld { table: &'static str, reason: String },
}
