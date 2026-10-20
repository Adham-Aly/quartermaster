//! Instants in the database: `INTEGER` milliseconds since the Unix epoch, UTC, in `*_at` columns.

use crate::StoreError;
use jiff::Timestamp;

/// The column value for an instant.
pub fn to_millis(at: Timestamp) -> i64 {
    at.as_millisecond()
}

/// The instant a column value stands for.
pub fn from_millis(column: &'static str, ms: i64) -> Result<Timestamp, StoreError> {
    Timestamp::from_millisecond(ms).map_err(|e| StoreError::Invalid {
        column,
        message: e.to_string(),
    })
}
