//! Durable deferred starts. Transitions are compare-and-swaps from pending only.
//!
//! `mark_started` MUST run in the task creation transaction. There is deliberately no
//! intermediate claimed state to strand on a crash. References in payload are seq-pinned
//! by core; missing/forgotten references fail closed rather than following a reused ID.
use crate::{StoreError, seq, time::to_millis};
use jiff::Timestamp;
use qm_types::ids::{AgentId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug)]
pub struct DeferredRow {
    pub id: i64,
    pub owner: AgentId,
    pub owner_seq: i64,
    pub trigger: String,
    pub payload: String,
    pub status: String,
    pub task: Option<TaskId>,
    pub note: Option<String>,
}
const COLUMNS: &str = "id, owner_agent_id, owner_seq, trigger_json, payload, status, task_id, note";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeferredRow> {
    Ok(DeferredRow {
        id: r.get(0)?,
        owner: r.get(1)?,
        owner_seq: r.get(2)?,
        trigger: r.get(3)?,
        payload: r.get(4)?,
        status: r.get(5)?,
        task: r.get(6)?,
        note: r.get(7)?,
    })
}

pub fn insert(
    conn: &Connection,
    owner: AgentId,
    owner_seq: i64,
    trigger: &str,
    payload: &str,
    at: Timestamp,
) -> Result<i64, StoreError> {
    let id = i64::from(crate::ids::fresh(conn, "deferred_starts", 1..=u32::MAX)?);
    conn.execute("INSERT INTO deferred_starts (id, seq, owner_agent_id, owner_seq, trigger_json, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, seq::next(at), owner, owner_seq, trigger, payload, to_millis(at)])?;
    Ok(id)
}
pub fn get(conn: &Connection, id: i64) -> Result<Option<DeferredRow>, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM deferred_starts WHERE id = ?1"),
            [id],
            row,
        )
        .optional()?)
}
pub fn list(
    conn: &Connection,
    owner: Option<AgentId>,
    pending_only: bool,
) -> Result<Vec<DeferredRow>, StoreError> {
    Ok(conn.prepare(&format!("SELECT {COLUMNS} FROM deferred_starts WHERE (?1 IS NULL OR (owner_agent_id = ?1 AND owner_seq = (SELECT seq FROM agents WHERE id = ?1))) AND (?2 = 0 OR status = 'pending') ORDER BY seq"))?
        .query_map(params![owner, pending_only], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
}
pub fn cancel(
    conn: &Connection,
    id: i64,
    owner: AgentId,
    at: Timestamp,
) -> Result<bool, StoreError> {
    Ok(conn.execute("UPDATE deferred_starts SET status = 'cancelled', finished_at = ?3 WHERE id = ?1 AND owner_agent_id = ?2 AND owner_seq = (SELECT seq FROM agents WHERE id = ?2) AND status = 'pending'", params![id, owner, to_millis(at)])? == 1)
}
pub fn fail(conn: &Connection, id: i64, note: &str, at: Timestamp) -> Result<bool, StoreError> {
    Ok(conn.execute("UPDATE deferred_starts SET status = 'failed', note = ?2, finished_at = ?3 WHERE id = ?1 AND status = 'pending'", params![id, note, to_millis(at)])? == 1)
}
/// Call inside the transaction that inserts `task`; false means the ENTIRE start must roll back.
pub fn mark_started(
    conn: &Connection,
    id: i64,
    task: TaskId,
    at: Timestamp,
) -> Result<bool, StoreError> {
    Ok(conn.execute("UPDATE deferred_starts SET status = 'started', task_id = ?2, finished_at = ?3 WHERE id = ?1 AND status = 'pending'", params![id, task, to_millis(at)])? == 1)
}
