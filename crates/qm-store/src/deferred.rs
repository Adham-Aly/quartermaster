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

#[cfg(test)]
mod tests {
    use super::*;
    fn seed(conn: &Connection) {
        conn.execute_batch("INSERT INTO agents (id, role, home, status, created_at, seq) VALUES (1, 'quartermaster', '/h', 'idle', 0, 1);
            INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, merge_method, created_at, seq) VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 0, 1);").unwrap();
    }
    fn task(conn: &Connection) -> TaskId {
        conn.execute_batch("INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, branch, status, created_at, seq) VALUES (1, 1, 1, 'research', 'child', 'goal', 'direct_pr', 'child', 'queued', 0, 1);").unwrap();
        TaskId::new(1).unwrap()
    }
    #[test]
    fn start_and_consumption_rollback_together_and_cancel_always_wins_before_commit() {
        let mut conn = crate::connection::open_memory().unwrap();
        crate::schema::migrate(&mut conn).unwrap();
        seed(&conn);
        let at = Timestamp::now();
        let owner = AgentId::new(1);
        let id = insert(&conn, owner, 1, "{}", "{\"complete_request\":true}", at).unwrap();
        {
            let tx = conn.transaction().unwrap();
            assert!(mark_started(&tx, id, task(&tx), at).unwrap());
            // Simulates a failure/crash before the record transaction commits.
        }
        assert_eq!(get(&conn, id).unwrap().unwrap().status, "pending");
        let count: i64 = conn
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(!cancel(&conn, id, AgentId::new(2), at).unwrap());
        assert!(cancel(&conn, id, owner, at).unwrap());
        {
            let tx = conn.transaction().unwrap();
            let child = task(&tx);
            assert!(!mark_started(&tx, id, child, at).unwrap());
            // Start's caller must (and does) roll back when the CAS loses to cancel.
        }
        assert!(!fail(&conn, id, "late error", at).unwrap());
        assert_eq!(get(&conn, id).unwrap().unwrap().status, "cancelled");
        let next = insert(&conn, owner, 1, "{}", "{}", at).unwrap();
        let tx = conn.transaction().unwrap();
        let child = task(&tx);
        assert!(mark_started(&tx, next, child, at).unwrap());
        assert!(!mark_started(&tx, next, child, at).unwrap());
        tx.commit().unwrap();
        assert!(!cancel(&conn, next, owner, at).unwrap());
        assert_eq!(get(&conn, next).unwrap().unwrap().task, Some(child));
    }
    #[test]
    fn pending_and_terminal_records_survive_real_mirror_restart() {
        let dir = std::env::temp_dir().join(format!(
            "qm-deferred-{}-{}",
            std::process::id(),
            Timestamp::now().as_microsecond()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let owner = AgentId::new(1);
        let at = Timestamp::now();
        let (pending, cancelled, started);
        {
            let (conn, mut mirror) = crate::mirror::Mirror::open(&path).unwrap();
            seed(&conn);
            pending = insert(&conn, owner, 1, "{}", "{\"words\":\"keep exactly\"}", at).unwrap();
            cancelled = insert(&conn, owner, 1, "{}", "{}", at).unwrap();
            assert!(cancel(&conn, cancelled, owner, at).unwrap());
            started = insert(&conn, owner, 1, "{}", "{}", at).unwrap();
            assert!(mark_started(&conn, started, task(&conn), at).unwrap());
            mirror.flush(&conn).unwrap();
        }
        {
            let (conn, _) = crate::mirror::Mirror::open(&path).unwrap();
            assert_eq!(
                get(&conn, pending).unwrap().unwrap().payload,
                "{\"words\":\"keep exactly\"}"
            );
            assert_eq!(get(&conn, cancelled).unwrap().unwrap().status, "cancelled");
            assert_eq!(get(&conn, started).unwrap().unwrap().status, "started");
            assert_eq!(list(&conn, Some(owner), true).unwrap().len(), 1);
            assert!(!mark_started(&conn, started, TaskId::new(1).unwrap(), at).unwrap());
            assert_ne!(insert(&conn, owner, 1, "{}", "{}", at).unwrap(), started);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
