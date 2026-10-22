//! Decision holds.
//!
//! A hold is a held action of kind `decision` (`qm_store::held`); this module adds what the
//! core's `decisions` module needs beyond `held.rs`: the pending decision of a task, and the
//! supervisor's summary from `present_hold`, which changes the pending row's `summary` field only
//! (the app-written fields never change).

use crate::StoreError;
use qm_types::ids::{HeldActionId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// The task's pending decision card, if any (at most one is ever pending per task).
pub fn pending_for(conn: &Connection, task: TaskId) -> Result<Option<HeldActionId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM held_actions
             WHERE task_id = ?1 AND kind = 'decision' AND status = 'pending'
             ORDER BY seq DESC LIMIT 1",
            params![task],
            |r| r.get(0),
        )
        .optional()?)
}

/// Sets the supervisor's summary on a pending decision; returns whether the row was pending.
/// `summary` is already neutralised by the caller.
pub fn set_summary(
    conn: &Connection,
    held: HeldActionId,
    summary: &str,
) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "UPDATE held_actions SET payload = json_set(payload, '$.summary', ?2)
         WHERE id = ?1 AND kind = 'decision' AND status = 'pending'",
        params![held, summary],
    )? == 1)
}
