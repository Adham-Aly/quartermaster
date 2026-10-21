//! Crewmates' background jobs.
//!
//! A job's row is written from the engine's `JobStarted` and closed from its `JobFinished`. Its
//! process row (written by the supervisor's recorder) is linked by whichever write comes second:
//! [`insert_started`] links a process recorded before the job existed.
//!
//! A job label is unique per agent (`UNIQUE (agent_id, label)`), but an engine may hand out the
//! same label again in a later session (the provider engine counts `J1`, `J2`, … per session). A
//! finished job whose label comes round again is renamed `J3#<row id>` first, so the new job gets
//! a row of its own (and its own wake: the wake's dedupe key names the row).

use crate::StoreError;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::ids::{AgentId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// A job as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRow {
    pub id: i64,
    pub agent: AgentId,
    pub task: TaskId,
    pub label: String,
    pub command: String,
    pub log_path: Option<PathBuf>,
    pub started_at: Timestamp,
    pub exited_at: Option<Timestamp>,
    pub exit_code: Option<i32>,
}

/// Records a started job and links its process row; returns the job's row ID. Idempotent for a
/// job that is already recorded as running.
pub fn insert_started(
    conn: &Connection,
    agent: AgentId,
    task: TaskId,
    label: &str,
    command: &str,
    log_path: Option<&Path>,
    at: Timestamp,
) -> Result<i64, StoreError> {
    if let Some(running) = running_by_label(conn, agent, label)? {
        return Ok(running.id);
    }
    // A finished job with the same label: keep it under a name of its own.
    conn.execute(
        "UPDATE jobs SET label = label || '#' || id
         WHERE agent_id = ?1 AND label = ?2 AND exited_at IS NOT NULL",
        params![agent, label],
    )?;
    let id: i64 = conn.query_row(
        "INSERT INTO jobs (agent_id, task_id, label, command, log_path, started_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
        params![
            agent,
            task,
            label,
            command,
            log_path.map(|p| p.to_string_lossy().into_owned()),
            to_millis(at),
            seq::next(at),
        ],
        |r| r.get(0),
    )?;
    conn.execute(
        "UPDATE processes SET job_id = ?1
         WHERE owner = 'job' AND agent_id = ?2 AND label = ?3 AND job_id IS NULL",
        params![id, agent, label],
    )?;
    Ok(id)
}

const COLUMNS: &str =
    "id, agent_id, task_id, label, command, log_path, started_at, exited_at, exit_code";

type Raw = (
    i64,
    AgentId,
    TaskId,
    String,
    String,
    Option<String>,
    i64,
    Option<i64>,
    Option<i32>,
);

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn build(
    (id, agent, task, label, command, log_path, started, exited, exit_code): Raw,
) -> Result<JobRow, StoreError> {
    Ok(JobRow {
        id,
        agent,
        task,
        label,
        command,
        log_path: log_path.map(PathBuf::from),
        started_at: from_millis("jobs.started_at", started)?,
        exited_at: exited
            .map(|e| from_millis("jobs.exited_at", e))
            .transpose()?,
        exit_code,
    })
}

/// The running job of `agent` with this label.
pub fn running_by_label(
    conn: &Connection,
    agent: AgentId,
    label: &str,
) -> Result<Option<JobRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM jobs
                 WHERE agent_id = ?1 AND label = ?2 AND exited_at IS NULL"
            ),
            params![agent, label],
            raw,
        )
        .optional()?;
    raw.map(build).transpose()
}

/// Closes the running job of `agent` with this label; returns it as it now stands, or `None` if no
/// such job was running (a repeat `JobFinished`).
pub fn finish(
    conn: &Connection,
    agent: AgentId,
    label: &str,
    exit_code: Option<i32>,
    at: Timestamp,
) -> Result<Option<JobRow>, StoreError> {
    let Some(job) = running_by_label(conn, agent, label)? else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE jobs SET exited_at = ?2, exit_code = ?3 WHERE id = ?1",
        params![job.id, to_millis(at), exit_code],
    )?;
    Ok(Some(JobRow {
        exited_at: Some(at),
        exit_code,
        ..job
    }))
}

/// The helper agents of `task` still running: jobs labelled `qm_types::ids::HELPER_JOB_PREFIX`.
pub fn running_helpers(conn: &Connection, task: TaskId) -> Result<Vec<JobRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM jobs WHERE task_id = ?1 AND exited_at IS NULL \
         AND substr(label, 1, length(?2)) = ?2 ORDER BY seq"
    ))?;
    stmt.query_map(params![task, qm_types::ids::HELPER_JOB_PREFIX], raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(build)
        .collect()
}

/// The jobs of `agent` still running.
pub fn running(conn: &Connection, agent: AgentId) -> Result<Vec<JobRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM jobs WHERE agent_id = ?1 AND exited_at IS NULL ORDER BY seq"
    ))?;
    stmt.query_map(params![agent], raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(build)
        .collect()
}

/// Marks every job of `agent` still recorded as running as ended without an exit code (its
/// session is gone and nothing will report it). Returns how many.
pub fn close_running(
    conn: &Connection,
    agent: AgentId,
    at: Timestamp,
) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE jobs SET exited_at = ?2 WHERE agent_id = ?1 AND exited_at IS NULL",
        params![agent, to_millis(at)],
    )?)
}

/// A recorded process group of a task that never saw its exit: a job or setup group to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenGroup {
    pub pgid: i32,
    pub pid: u32,
    pub started_at: Timestamp,
}

/// The job and setup groups of `task` (and its crewmate `agent`) with no recorded exit.
pub fn open_groups_of_task(
    conn: &Connection,
    task: TaskId,
    agent: Option<AgentId>,
) -> Result<Vec<OpenGroup>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT pgid, pid, started_at FROM processes
         WHERE ended_at IS NULL
           AND ((owner = 'setup' AND task_id = ?1) OR (owner = 'job' AND agent_id = ?2))
         ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![task, agent], |r| {
            Ok((
                r.get::<_, i32>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(pgid, pid, started)| {
            Ok(OpenGroup {
                pgid,
                pid,
                started_at: from_millis("processes.started_at", started)?,
            })
        })
        .collect()
}
