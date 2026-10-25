//! What the retention sweep (`qm_core::retention`) must know before it deletes anything: which
//! tasks are open and what they build on or retry, which closed tasks' reports the user hasn't read
//! yet, and which provider-engine session each live agent is on.

use crate::StoreError;
use qm_types::domain::{EngineKind, TaskKind};
use qm_types::ids::{SecondmateId, SessionRef, TaskId, TaskLabelId};
use rusqlite::Connection;

/// An open crewmate, where its files are filed, and the crewmates whose files it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenTask {
    pub id: TaskId,
    /// The task whose folders hold its artifacts and scratchpad.
    pub folder_label: TaskLabelId,
    pub kind: TaskKind,
    pub builds_on: Option<TaskId>,
    pub retry_of: Option<TaskId>,
}

/// Whose transcript folder an agent's session lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Owner {
    Seat,
    Secondmate(SecondmateId),
    Task(TaskId),
}

/// A live agent's current session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSession {
    pub owner: Owner,
    pub engine: Option<EngineKind>,
    pub session_ref: Option<SessionRef>,
}

/// Every fact the sweep decides by, read in one go so the decisions agree with each other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Closed tasks whose report the user hasn't opened or dismissed yet: its card still waits in
    /// Actions, so its page must stay.
    pub unread_reports: Vec<TaskId>,
    pub open: Vec<OpenTask>,
    pub sessions: Vec<LiveSession>,
}

/// Reads the [`Facts`]. `report_kinds` are the kinds whose report has a card until it is read
/// (`qm_core::reports::api::has_report_card`): a closed one keeps its card only when it answered.
pub fn facts(conn: &Connection, report_kinds: &[TaskKind]) -> Result<Facts, StoreError> {
    let kinds =
        serde_json::Value::from(report_kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>())
            .to_string();
    let unread_reports = conn
        .prepare(
            "SELECT id FROM tasks
             WHERE status = 'closed' AND outcome = 'answered'
               AND kind IN (SELECT value FROM json_each(?1)) AND reported_at IS NOT NULL
               AND (report_read_at IS NULL OR report_read_at < reported_at)
             ORDER BY seq",
        )?
        .query_map([kinds], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let open = conn
        .prepare(
            "SELECT id, kind, builds_on, retry_of, COALESCE(folder_label_id, id) FROM tasks
             WHERE status <> 'closed' ORDER BY seq",
        )?
        .query_map([], |r| {
            Ok(OpenTask {
                id: r.get(0)?,
                folder_label: r.get(4)?,
                kind: r.get(1)?,
                builds_on: r.get(2)?,
                retry_of: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let rows = conn
        .prepare(
            "SELECT a.role, a.secondmate_id, a.task_id, a.engine, a.session_ref FROM agents a
             LEFT JOIN tasks t ON t.id = a.task_id
             WHERE a.retired_at IS NULL AND (a.task_id IS NULL OR t.status <> 'closed')
             ORDER BY a.seq",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<SecondmateId>>(1)?,
                r.get::<_, Option<TaskId>>(2)?,
                r.get::<_, Option<EngineKind>>(3)?,
                r.get::<_, Option<SessionRef>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let sessions = rows
        .into_iter()
        .filter_map(|(role, secondmate, task, engine, session_ref)| {
            let owner = match (role.as_str(), secondmate, task) {
                ("quartermaster", _, _) => Owner::Seat,
                ("secondmate", Some(id), _) => Owner::Secondmate(id),
                ("crewmate", _, Some(id)) => Owner::Task(id),
                _ => return None,
            };
            Some(LiveSession {
                owner,
                engine,
                session_ref,
            })
        })
        .collect();
    Ok(Facts {
        unread_reports,
        open,
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_reports_are_closed_answered_card_kinds_not_yet_read() {
        let mut conn = crate::connection::open_memory().unwrap();
        crate::schema::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode,
                 status, outcome, created_at, reported_at, report_read_at, closed_at, seq) VALUES
             (1, 1, 1, 'lookup', 't', 'g', 'folder', 'closed', 'answered', 1, 2, NULL, 3, 1),
             (2, 1, 1, 'research', 't', 'g', 'folder', 'closed', 'answered', 1, 2, 3, 4, 2),
             (3, 1, 1, 'teach', 't', 'g', 'folder', 'closed', 'answered', 1, 5, 3, 6, 3),
             (4, 1, 1, 'lookup', 't', 'g', 'folder', 'closed', 'stopped', 1, 2, NULL, 3, 4),
             (5, 1, 1, 'lookup', 't', 'g', 'folder', 'reported', NULL, 1, 2, NULL, NULL, 5),
             (6, 1, 1, 'plan', 't', 'g', 'folder', 'closed', 'answered', 1, 2, NULL, 3, 6),
             (7, 1, 1, 'lookup', 't', 'g', 'folder', 'closed', 'answered', 1, NULL, NULL, 3, 7);",
        )
        .unwrap();
        let kinds = [TaskKind::Lookup, TaskKind::Research, TaskKind::Teach];
        let facts = facts(&conn, &kinds).unwrap();
        // Unread (1) and reported again since it was read (3); not read (2), stopped (4), open
        // (5), a kind without a card (6) or never reported (7).
        let ids: Vec<u32> = facts.unread_reports.iter().map(|t| t.get()).collect();
        assert_eq!(ids, [1, 3]);
        assert_eq!(facts.open.len(), 1);
    }
}
