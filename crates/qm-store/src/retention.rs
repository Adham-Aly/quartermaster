//! What the retention sweep (`qm_core::retention`) must know before it deletes anything: which
//! tasks still have rows, which are open and what they build on or retry, and which
//! provider-engine session each live agent is on.

use crate::StoreError;
use qm_types::domain::{EngineKind, TaskKind};
use qm_types::ids::{SecondmateId, SessionRef, TaskId};
use rusqlite::Connection;

/// An open task and the tasks whose files it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenTask {
    pub id: TaskId,
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
    /// All task rows, including closed tasks; their IDs cannot yet be reused.
    pub retained_tasks: Vec<TaskId>,
    pub open: Vec<OpenTask>,
    pub sessions: Vec<LiveSession>,
}

/// Reads the [`Facts`].
pub fn facts(conn: &Connection) -> Result<Facts, StoreError> {
    let retained_tasks = conn
        .prepare("SELECT id FROM tasks ORDER BY seq")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let open = conn
        .prepare(
            "SELECT id, kind, builds_on, retry_of FROM tasks WHERE status <> 'closed' ORDER BY seq",
        )?
        .query_map([], |r| {
            Ok(OpenTask {
                id: r.get(0)?,
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
        retained_tasks,
        open,
        sessions,
    })
}
