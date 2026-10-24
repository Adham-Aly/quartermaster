//! The context block's task line, verification's part: "committed locally · in review by C-0046",
//! "held · 3 repair attempts used · decision is with you", "held · review still has findings after
//! 2 re-reviews · decision is with you", "held · the review is blocked · decision is with you",
//! "stuck · decision is with you", "review passed · pushing". The words are
//! `qm_store::verification::task_line`'s, so the context builder can read them in its own query.

use qm_store::StoreError;
use qm_types::ids::TaskId;
use rusqlite::Connection;

/// Verification's part of `task`'s line, or `None` when it has nothing to say.
pub fn task_line(conn: &Connection, task: TaskId) -> Result<Option<String>, StoreError> {
    qm_store::verification::task_line(conn, task)
}
