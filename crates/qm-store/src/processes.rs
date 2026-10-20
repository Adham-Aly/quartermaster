//! Recorded process groups.
//!
//! Every process group the supervisor starts is written here when it starts
//! ([`insert_started`]) and closed when its leader exits ([`mark_exited`]). At the next startup,
//! [`open_groups`] lists the rows that never saw an exit (the app was killed, or a group outlived
//! its leader), the core checks which of them are still alive, and [`close_stale`] retires the
//! ones that are gone. A group is identified by its leader's start time as the OS reported it,
//! never by its program name.
//!
//! A job's row names its agent and job label (a `CHECK`), and its `job_id` is linked by whichever
//! write comes second: this insert looks the job up by agent and label, and the engine's
//! `JobStarted` handler links rows inserted before the job existed.
//!
//! This module speaks plain columns rather than the supervisor's types, so the store stays free of
//! the engine layer; `qm_core::processes` converts.

use crate::StoreError;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::ProcessOwnerKind;
use qm_types::ids::{AgentId, TaskId};
use rusqlite::{Connection, params};

/// A process group to record as started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewProcess {
    pub pgid: i32,
    pub pid: u32,
    pub program: String,
    pub owner: ProcessOwnerKind,
    pub agent: Option<AgentId>,
    pub task: Option<TaskId>,
    /// The job label for a job's process; a free label for an app process.
    pub label: Option<String>,
    /// The leader's start time as the OS reported it.
    pub started_at: Timestamp,
}

/// A recorded group with no exit yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenProcess {
    pub pgid: i32,
    pub pid: u32,
    pub program: String,
    pub owner: ProcessOwnerKind,
    pub agent: Option<AgentId>,
    pub task: Option<TaskId>,
    pub label: Option<String>,
    pub started_at: Timestamp,
}

/// How a recorded group ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessEnd {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
    },
    /// Found gone at startup: how it ended is unknown.
    Unknown,
}

/// Records a started group. A job's process is linked to its `jobs` row if that row exists
/// already. An agent or task that has no row (the startup probe's throwaway sessions, a
/// developer run) is recorded as unknown rather than failing the foreign key: the group is still
/// found by the leftover check.
pub fn insert_started(conn: &Connection, process: &NewProcess) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO processes (pgid, pid, program, owner, agent_id, job_id, task_id, label, started_at)
         VALUES (?1, ?2, ?3, ?4,
                 CASE WHEN ?4 = 'job' THEN ?5 ELSE (SELECT id FROM agents WHERE id = ?5) END,
                 CASE WHEN ?4 = 'job'
                      THEN (SELECT id FROM jobs WHERE agent_id = ?5 AND label = ?7)
                 END,
                 (SELECT id FROM tasks WHERE id = ?6), ?7, ?8)",
        params![
            process.pgid,
            process.pid,
            process.program,
            process.owner,
            process.agent,
            process.task,
            process.label,
            to_millis(process.started_at),
        ],
    )?;
    Ok(())
}

/// Closes the open row of `pgid` with how its leader ended. `stop_requested` says whether the app
/// asked it to stop (otherwise it was a crash). Returns whether a row was closed.
pub fn mark_exited(
    conn: &Connection,
    pgid: i32,
    end: ProcessEnd,
    stop_requested: bool,
    at: Timestamp,
) -> Result<bool, StoreError> {
    let (code, signal) = match end {
        ProcessEnd::Exited { code } => (Some(code), None),
        ProcessEnd::Signaled { signal } => (None, Some(signal)),
        ProcessEnd::Unknown => (None, None),
    };
    let changed = conn.execute(
        "UPDATE processes
            SET ended_at = ?2, exit_code = ?3, signal = ?4,
                stop_requested = MAX(stop_requested, ?5)
          WHERE pgid = ?1 AND ended_at IS NULL",
        params![pgid, to_millis(at), code, signal, i64::from(stop_requested)],
    )?;
    Ok(changed > 0)
}

/// Marks the open row of `pgid` as asked to stop, before the stop begins.
pub fn mark_stop_requested(conn: &Connection, pgid: i32) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE processes SET stop_requested = 1 WHERE pgid = ?1 AND ended_at IS NULL",
        params![pgid],
    )?;
    Ok(())
}

/// Every recorded group with no exit, oldest first.
pub fn open_groups(conn: &Connection) -> Result<Vec<OpenProcess>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT pgid, pid, program, owner, agent_id, task_id, label, started_at
           FROM processes
          WHERE ended_at IS NULL
          ORDER BY started_at, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i32>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, ProcessOwnerKind>(3)?,
            r.get::<_, Option<AgentId>>(4)?,
            r.get::<_, Option<TaskId>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;
    let mut open = Vec::new();
    for row in rows {
        let (pgid, pid, program, owner, agent, task, label, started_at) = row?;
        open.push(OpenProcess {
            pgid,
            pid,
            program,
            owner,
            agent,
            task,
            label,
            started_at: from_millis("processes.started_at", started_at)?,
        });
    }
    Ok(open)
}

/// Retires the open rows of groups found gone at startup.
pub fn close_stale(conn: &Connection, pgids: &[i32], at: Timestamp) -> Result<(), StoreError> {
    for &pgid in pgids {
        mark_exited(conn, pgid, ProcessEnd::Unknown, false, at)?;
    }
    Ok(())
}

/// The port blocks (`QM_PORT_BASE` values) of closed tasks, for the startup listener check.
pub fn closed_task_port_bases(conn: &Connection) -> Result<Vec<(TaskId, u16)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, port_base FROM tasks
          WHERE port_base IS NOT NULL
            AND (closed_at IS NOT NULL
                 OR status IN ('landed', 'closed', 'failed', 'stopped', 'discarded'))
          ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, TaskId>(0)?, r.get::<_, i64>(1)?)))?;
    let mut bases = Vec::new();
    for row in rows {
        let (task, base) = row?;
        let base = u16::try_from(base).map_err(|_| StoreError::Invalid {
            column: "tasks.port_base",
            message: format!("{base} is not a port"),
        })?;
        bases.push((task, base));
    }
    Ok(bases)
}
