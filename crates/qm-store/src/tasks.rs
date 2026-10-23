//! Tasks: their rows, status changes, links and the facts the watchdogs read.
//!
//! - [`insert`] draws the task's ID at random inside the caller's transaction (see [`crate::ids`]),
//!   with the branch the caller picked ([`first_free_branch`], in the same transaction).
//! - [`set_status`] and [`set_closed`] are compare-and-swaps: they fail if the row moved, and write
//!   the `task.status` event in the same transaction as the change.
//! - [`signs`] reads the watchdogs' signs of life in one query.

use crate::StoreError;
use crate::events::{self, EventRow, NewEvent};
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{
    DeliveryMode, TaskKind, TaskOutcome, TaskStatus, VerificationLevel, VerificationSource,
};
use qm_types::ids::space;
use qm_types::ids::{AgentId, CommitSha, EventId, MessageId, ProjectId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// The `events.kind` of a status change.
pub const STATUS_EVENT: &str = "task.status";

/// A task to record.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTask {
    pub project: ProjectId,
    pub supervisor: AgentId,
    pub kind: TaskKind,
    pub title: String,
    pub goal: String,
    pub user_words: String,
    pub user_words_message: Option<MessageId>,
    /// `Vec<String>` as JSON.
    pub instructions: serde_json::Value,
    /// `Vec<Constraint>` as JSON.
    pub constraints: serde_json::Value,
    pub tests_required: bool,
    pub builds_on: Option<TaskId>,
    pub parent: Option<TaskId>,
    pub retry_of: Option<TaskId>,
    pub escalations: u8,
    /// `Vec<LaterStep>` as JSON.
    pub later_steps: serde_json::Value,
    pub secondmate_request: Option<i64>,
    pub delivery: DeliveryMode,
    pub verification: Option<(VerificationLevel, VerificationSource)>,
    pub base_branch: Option<String>,
    pub base_ref: Option<String>,
    pub base_commit: Option<CommitSha>,
    /// The task's branch, named after its work ([`first_free_branch`]); `None` for a folder task.
    pub branch: Option<String>,
    pub port_base: Option<u16>,
    pub at: Timestamp,
}

/// A task as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskRow {
    pub id: TaskId,
    pub project: ProjectId,
    pub supervisor: AgentId,
    pub kind: TaskKind,
    pub title: String,
    pub goal: String,
    pub user_words: String,
    pub user_words_message: Option<MessageId>,
    pub instructions: serde_json::Value,
    pub constraints: serde_json::Value,
    pub tests_required: bool,
    pub builds_on: Option<TaskId>,
    pub parent: Option<TaskId>,
    pub retry_of: Option<TaskId>,
    pub later_steps: serde_json::Value,
    pub secondmate_request: Option<i64>,
    pub delivery: DeliveryMode,
    pub verification: Option<(VerificationLevel, VerificationSource)>,
    pub base_branch: Option<String>,
    pub base_ref: Option<String>,
    pub base_commit: Option<CommitSha>,
    /// The task's branch: empty for a folder task, which has none (the database holds NULL). Only
    /// git delivery reads it, and a folder task never reaches a git path.
    pub branch: String,
    pub port_base: Option<u16>,
    pub status: TaskStatus,
    pub outcome: Option<TaskOutcome>,
    pub waiting_on: Option<serde_json::Value>,
    pub pr_number: Option<i64>,
    pub head_commit: Option<CommitSha>,
    pub escalations: u8,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub reported_at: Option<Timestamp>,
    pub closed_at: Option<Timestamp>,
}

const COLUMNS: &str = "id, project_id, supervisor_id, kind, title, goal, user_words,
    user_words_message_id, instructions, constraints, tests_required, builds_on, parent_task_id,
    retry_of, later_steps, secondmate_request_id, delivery_mode, verification_level,
    verification_source, base_branch, base_ref, base_commit, branch, port_base, status, outcome,
    waiting_on, pr_number, head_commit, escalations, created_at, started_at, reported_at,
    closed_at";

fn json_text(column: &'static str, v: &serde_json::Value) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json { column, source })
}

fn json_value(column: &'static str, s: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(s).map_err(|source| StoreError::Json { column, source })
}

fn sha(column: &'static str, s: Option<String>) -> Result<Option<CommitSha>, StoreError> {
    s.map(|s| {
        s.parse::<CommitSha>().map_err(|e| StoreError::Invalid {
            column,
            message: e.to_string(),
        })
    })
    .transpose()
}

fn port(base: Option<i64>) -> Result<Option<u16>, StoreError> {
    base.map(|b| {
        u16::try_from(b).map_err(|_| StoreError::Invalid {
            column: "tasks.port_base",
            message: format!("{b} is not a port"),
        })
    })
    .transpose()
}

fn millis(column: &'static str, v: Option<i64>) -> Result<Option<Timestamp>, StoreError> {
    v.map(|ms| from_millis(column, ms)).transpose()
}

/// The raw columns of one row, read inside rusqlite's closure.
struct Raw {
    id: TaskId,
    project: ProjectId,
    supervisor: AgentId,
    kind: TaskKind,
    title: String,
    goal: String,
    user_words: String,
    user_words_message: Option<MessageId>,
    instructions: String,
    constraints: String,
    tests_required: bool,
    builds_on: Option<TaskId>,
    parent: Option<TaskId>,
    retry_of: Option<TaskId>,
    later_steps: String,
    secondmate_request: Option<i64>,
    delivery: DeliveryMode,
    level: Option<VerificationLevel>,
    source: Option<VerificationSource>,
    base_branch: Option<String>,
    base_ref: Option<String>,
    base_commit: Option<String>,
    branch: Option<String>,
    port_base: Option<i64>,
    status: TaskStatus,
    outcome: Option<TaskOutcome>,
    waiting_on: Option<String>,
    pr_number: Option<i64>,
    head_commit: Option<String>,
    escalations: i64,
    created_at: i64,
    started_at: Option<i64>,
    reported_at: Option<i64>,
    closed_at: Option<i64>,
}

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: r.get(0)?,
        project: r.get(1)?,
        supervisor: r.get(2)?,
        kind: r.get(3)?,
        title: r.get(4)?,
        goal: r.get(5)?,
        user_words: r.get(6)?,
        user_words_message: r.get(7)?,
        instructions: r.get(8)?,
        constraints: r.get(9)?,
        tests_required: r.get(10)?,
        builds_on: r.get(11)?,
        parent: r.get(12)?,
        retry_of: r.get(13)?,
        later_steps: r.get(14)?,
        secondmate_request: r.get(15)?,
        delivery: r.get(16)?,
        level: r.get(17)?,
        source: r.get(18)?,
        base_branch: r.get(19)?,
        base_ref: r.get(20)?,
        base_commit: r.get(21)?,
        branch: r.get(22)?,
        port_base: r.get(23)?,
        status: r.get(24)?,
        outcome: r.get(25)?,
        waiting_on: r.get(26)?,
        pr_number: r.get(27)?,
        head_commit: r.get(28)?,
        escalations: r.get(29)?,
        created_at: r.get(30)?,
        started_at: r.get(31)?,
        reported_at: r.get(32)?,
        closed_at: r.get(33)?,
    })
}

impl Raw {
    fn parse(self) -> Result<TaskRow, StoreError> {
        Ok(TaskRow {
            id: self.id,
            project: self.project,
            supervisor: self.supervisor,
            kind: self.kind,
            title: self.title,
            goal: self.goal,
            user_words: self.user_words,
            user_words_message: self.user_words_message,
            instructions: json_value("tasks.instructions", &self.instructions)?,
            constraints: json_value("tasks.constraints", &self.constraints)?,
            tests_required: self.tests_required,
            builds_on: self.builds_on,
            parent: self.parent,
            retry_of: self.retry_of,
            later_steps: json_value("tasks.later_steps", &self.later_steps)?,
            secondmate_request: self.secondmate_request,
            delivery: self.delivery,
            verification: self.level.zip(self.source),
            base_branch: self.base_branch,
            base_ref: self.base_ref,
            base_commit: sha("tasks.base_commit", self.base_commit)?,
            branch: self.branch.unwrap_or_default(),
            port_base: port(self.port_base)?,
            status: self.status,
            outcome: self.outcome,
            waiting_on: self
                .waiting_on
                .map(|w| json_value("tasks.waiting_on", &w))
                .transpose()?,
            pr_number: self.pr_number,
            head_commit: sha("tasks.head_commit", self.head_commit)?,
            escalations: u8::try_from(self.escalations).unwrap_or(u8::MAX),
            created_at: from_millis("tasks.created_at", self.created_at)?,
            started_at: millis("tasks.started_at", self.started_at)?,
            reported_at: millis("tasks.reported_at", self.reported_at)?,
            closed_at: millis("tasks.closed_at", self.closed_at)?,
        })
    }
}

fn collect(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<TaskRow>, StoreError> {
    stmt.query_map(params, raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(Raw::parse)
        .collect()
}

/// Records a task as `queued`, allocating its ID; returns it. Call inside the start transaction.
pub fn insert(conn: &Connection, task: &NewTask) -> Result<TaskId, StoreError> {
    let number = ids::fresh(conn, "tasks", space::TASK)?;
    let id = TaskId::new(number).ok_or_else(|| StoreError::Invalid {
        column: "tasks.id",
        message: format!("{number} is out of range for a task ID"),
    })?;
    let (level, source) = task.verification.unzip();
    conn.execute(
        "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, user_words,
             user_words_message_id, instructions, constraints, tests_required, builds_on,
             parent_task_id, retry_of, later_steps, secondmate_request_id, delivery_mode,
             verification_level, verification_source, base_branch, base_ref, base_commit, branch,
             port_base, status, escalations, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22, ?23, ?24, 'queued', ?25, ?26, ?27)",
        params![
            id,
            task.project,
            task.supervisor,
            task.kind,
            task.title,
            task.goal,
            task.user_words,
            task.user_words_message,
            json_text("tasks.instructions", &task.instructions)?,
            json_text("tasks.constraints", &task.constraints)?,
            i64::from(task.tests_required),
            task.builds_on,
            task.parent,
            task.retry_of,
            json_text("tasks.later_steps", &task.later_steps)?,
            task.secondmate_request,
            task.delivery,
            level,
            source,
            task.base_branch,
            task.base_ref,
            task.base_commit.as_ref().map(CommitSha::as_str),
            task.branch
                .as_deref()
                .filter(|_| task.delivery != DeliveryMode::Folder),
            task.port_base.map(i64::from),
            i64::from(task.escalations),
            to_millis(task.at),
            seq::next(task.at),
        ],
    )?;
    Ok(id)
}

/// One task.
pub fn load(conn: &Connection, task: TaskId) -> Result<Option<TaskRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"),
            params![task],
            raw,
        )
        .optional()?;
    raw.map(Raw::parse).transpose()
}

/// Every task not closed, oldest first.
pub fn open(conn: &Connection) -> Result<Vec<TaskRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM tasks WHERE status <> 'closed' ORDER BY seq"
    ))?;
    collect(&mut stmt, [])
}

/// Every task in `status`, oldest first.
pub fn with_status(conn: &Connection, status: TaskStatus) -> Result<Vec<TaskRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM tasks WHERE status = ?1 ORDER BY seq"
    ))?;
    collect(&mut stmt, params![status])
}

/// Every task, oldest first (for `find_tasks`, which filters in Rust).
pub fn all(conn: &Connection) -> Result<Vec<TaskRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM tasks ORDER BY seq"))?;
    collect(&mut stmt, [])
}

/// Appends one `task.*` event. Its payload also names the task's supervisor as `supervisor`, so an
/// orchestrator's block shows its own tasks' lines and outcomes (plan 5, step E).
#[allow(clippy::too_many_arguments)]
pub fn event(
    conn: &Connection,
    kind: &str,
    task: TaskId,
    agent: Option<AgentId>,
    summary: &str,
    mut payload: serde_json::Value,
    at: Timestamp,
) -> Result<EventId, StoreError> {
    let supervisor: Option<AgentId> = conn
        .query_row(
            "SELECT supervisor_id FROM tasks WHERE id = ?1",
            params![task],
            |r| r.get(0),
        )
        .optional()?;
    if let (Some(supervisor), Some(map)) = (supervisor, payload.as_object_mut()) {
        map.insert("supervisor".to_owned(), serde_json::json!(supervisor.get()));
    }
    events::insert(
        conn,
        &NewEvent {
            kind,
            agent,
            task: Some(task),
            turn: None,
            summary,
            payload,
            at,
        },
    )
}

fn moved(task: TaskId, from: TaskStatus) -> StoreError {
    StoreError::Invalid {
        column: "tasks.status",
        message: format!("{task} is no longer {from}"),
    }
}

/// Moves a task from `from` to `to` (never `closed`: see [`set_closed`]), with its `task.status`
/// event; fails if the row is no longer `from`. `preparing` sets `started_at`, `reported`
/// `reported_at`; leaving `waiting` clears `waiting_on`.
pub fn set_status(
    conn: &Connection,
    task: TaskId,
    from: TaskStatus,
    to: TaskStatus,
    at: Timestamp,
) -> Result<(), StoreError> {
    if to == TaskStatus::Closed {
        return Err(StoreError::Invalid {
            column: "tasks.status",
            message: "closing a task records its outcome; use set_closed".to_owned(),
        });
    }
    let changed = conn.execute(
        "UPDATE tasks SET status = ?3,
             started_at = CASE WHEN ?3 = 'preparing' THEN COALESCE(started_at, ?4) ELSE started_at END,
             reported_at = CASE WHEN ?3 = 'reported' THEN ?4 ELSE reported_at END,
             waiting_on = CASE WHEN ?3 = 'waiting' THEN waiting_on ELSE NULL END
         WHERE id = ?1 AND status = ?2",
        params![task, from, to, to_millis(at)],
    )?;
    if changed != 1 {
        return Err(moved(task, from));
    }
    status_event(conn, task, from, to, None, at)
}

/// Closes a task from `from` with its outcome and `closed_at`, and its `task.status` event.
pub fn set_closed(
    conn: &Connection,
    task: TaskId,
    from: TaskStatus,
    outcome: TaskOutcome,
    at: Timestamp,
) -> Result<(), StoreError> {
    let changed = conn.execute(
        "UPDATE tasks SET status = 'closed', outcome = ?3, closed_at = ?4, waiting_on = NULL
         WHERE id = ?1 AND status = ?2",
        params![task, from, outcome, to_millis(at)],
    )?;
    if changed != 1 {
        return Err(moved(task, from));
    }
    status_event(conn, task, from, TaskStatus::Closed, Some(outcome), at)
}

fn status_event(
    conn: &Connection,
    task: TaskId,
    from: TaskStatus,
    to: TaskStatus,
    outcome: Option<TaskOutcome>,
    at: Timestamp,
) -> Result<(), StoreError> {
    event(
        conn,
        STATUS_EVENT,
        task,
        None,
        &format!("{task} {from} → {to}"),
        serde_json::json!({ "from": from, "to": to, "outcome": outcome }),
        at,
    )?;
    Ok(())
}

/// Records a declared wait (`{"task": "T-0040"}`, `{"external": "…"}`, `{"decision": 12}`).
pub fn set_waiting_on(
    conn: &Connection,
    task: TaskId,
    waiting_on: Option<&serde_json::Value>,
) -> Result<(), StoreError> {
    let text = waiting_on
        .map(|w| json_text("tasks.waiting_on", w))
        .transpose()?;
    conn.execute(
        "UPDATE tasks SET waiting_on = ?2 WHERE id = ?1",
        params![task, text],
    )?;
    Ok(())
}

/// Records the task's head commit (a done report).
pub fn set_head_commit(
    conn: &Connection,
    task: TaskId,
    head: &CommitSha,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET head_commit = ?2 WHERE id = ?1",
        params![task, head.as_str()],
    )?;
    Ok(())
}

/// The first of `candidates` no task row holds, open or closed; `None` when every one is taken.
/// Call inside the transaction that records the task, so no other start takes it in between.
pub fn first_free_branch(
    conn: &Connection,
    candidates: &[String],
) -> Result<Option<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT 1 FROM tasks WHERE branch = ?1")?;
    for name in candidates {
        if !stmt.exists(params![name])? {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

/// The task whose branch is `branch`, open or closed.
pub fn with_branch(conn: &Connection, branch: &str) -> Result<Option<TaskRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM tasks WHERE branch = ?1"),
            params![branch],
            raw,
        )
        .optional()?;
    raw.map(Raw::parse).transpose()
}

/// Renames a task's branch from `from` to `to`, only while it is still `from` (prepare's re-pick,
/// before any worktree or commit exists). Returns whether the row changed.
pub fn set_branch(
    conn: &Connection,
    task: TaskId,
    from: &str,
    to: &str,
) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE tasks SET branch = ?3 WHERE id = ?1 AND branch = ?2",
        params![task, from, to],
    )?;
    Ok(changed == 1)
}

/// Records a base settled again (a queued task's re-route): the typed branch, the ref and the
/// commit.
pub fn set_base(
    conn: &Connection,
    task: TaskId,
    base_branch: Option<&str>,
    base_ref: &str,
    base_commit: &CommitSha,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET base_branch = ?2, base_ref = ?3, base_commit = ?4 WHERE id = ?1",
        params![task, base_branch, base_ref, base_commit.as_str()],
    )?;
    Ok(())
}

/// The port blocks open tasks hold.
pub fn ports_in_use(conn: &Connection) -> Result<Vec<u16>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT port_base FROM tasks WHERE port_base IS NOT NULL AND status <> 'closed'",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|b| port(Some(b)).map(Option::unwrap_or_default))
        .collect()
}

/// The tasks of a project that aren't closed yet, with their kinds, oldest first. A folder project
/// checks these before a code-writing task starts in it.
pub fn open_in_project(
    conn: &Connection,
    project: ProjectId,
) -> Result<Vec<(TaskId, TaskKind)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, kind FROM tasks WHERE project_id = ?1 AND status <> 'closed' ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![project], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every open task working in place (folder delivery), across all projects: its ID, its
/// crewmate's folder and its kind, in creation order.
pub fn open_in_place(
    conn: &Connection,
) -> Result<Vec<(TaskId, std::path::PathBuf, TaskKind)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, a.home, t.kind FROM tasks AS t JOIN agents AS a ON a.task_id = t.id
         WHERE t.status <> 'closed' AND t.delivery_mode = 'folder' ORDER BY t.seq",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                std::path::PathBuf::from(r.get::<_, String>(1)?),
                r.get(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The port blocks of tasks closed since `since`, newest first, at most `limit`.
pub fn recently_closed_ports(
    conn: &Connection,
    since: Timestamp,
    limit: usize,
) -> Result<Vec<(TaskId, u16)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, port_base FROM tasks
         WHERE status = 'closed' AND port_base IS NOT NULL AND closed_at >= ?1
         ORDER BY closed_at DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(
            params![to_millis(since), i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| Ok((r.get::<_, TaskId>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(t, b)| Ok((t, port(Some(b))?.unwrap_or_default())))
        .collect()
}

/// The watchdogs' signs of life that the database holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Signs {
    /// When the newest still-running job of the agent started.
    pub newest_running_job: Option<Timestamp>,
    /// Wakes waiting to be delivered to the crewmate.
    pub pending_wakes: u32,
    /// A pending held action for the task: a question waiting on you.
    pub question_waiting: bool,
    /// The task is in a declared wait.
    pub waiting: bool,
}

/// The signs of life for `task` and its crewmate `agent`, in one query.
pub fn signs(conn: &Connection, task: TaskId, agent: AgentId) -> Result<Signs, StoreError> {
    let (job, wakes, held, waiting): (Option<i64>, i64, bool, bool) = conn.query_row(
        "SELECT
             (SELECT MAX(started_at) FROM jobs WHERE agent_id = ?2 AND exited_at IS NULL),
             (SELECT COUNT(*) FROM wakes AS w JOIN agents AS a ON a.id = w.target_agent_id
              WHERE w.target_agent_id = ?2 AND w.delivered_at IS NULL
                AND w.consumed_at IS NULL AND a.retired_at IS NULL),
             EXISTS (SELECT 1 FROM held_actions WHERE task_id = ?1 AND status = 'pending'),
             EXISTS (SELECT 1 FROM tasks WHERE id = ?1 AND status = 'waiting')",
        params![task, agent],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(Signs {
        newest_running_job: millis("jobs.started_at", job)?,
        pending_wakes: u32::try_from(wakes).unwrap_or(u32::MAX),
        question_waiting: held,
        waiting,
    })
}

/// Whether a pending `decision` held action exists for the task (the stuck notice is written
/// once).
pub fn pending_decision(conn: &Connection, task: TaskId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM held_actions
                        WHERE task_id = ?1 AND kind = 'decision' AND status = 'pending')",
        params![task],
        |r| r.get(0),
    )?)
}

/// The tasks in a declared wait on `task`, with their crewmates.
pub fn waiting_on_task(
    conn: &Connection,
    task: TaskId,
) -> Result<Vec<(TaskId, Option<AgentId>)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, (SELECT a.id FROM agents AS a WHERE a.task_id = t.id AND a.role = 'crewmate'
                       ORDER BY a.seq DESC LIMIT 1)
         FROM tasks AS t
         WHERE t.status = 'waiting' AND json_extract(t.waiting_on, '$.task') = ?1
         ORDER BY t.seq",
    )?;
    Ok(stmt
        .query_map(params![task.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A blocker of a task, with what decides whether it has finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub task: TaskId,
    pub kind: TaskKind,
    pub status: TaskStatus,
}

/// The tasks blocking `task` (`task_links.blocked = task`).
pub fn blockers_of(conn: &Connection, task: TaskId) -> Result<Vec<Blocker>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.kind, t.status FROM task_links AS l JOIN tasks AS t ON t.id = l.blocker
         WHERE l.blocked = ?1 ORDER BY t.seq",
    )?;
    Ok(stmt
        .query_map(params![task], |r| {
            Ok(Blocker {
                task: r.get(0)?,
                kind: r.get(1)?,
                status: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The tasks `blocker` blocks, with their status and crewmate.
pub fn blocked_by(
    conn: &Connection,
    blocker: TaskId,
) -> Result<Vec<(TaskId, TaskStatus, Option<AgentId>)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.status, (SELECT a.id FROM agents AS a WHERE a.task_id = t.id
                                 AND a.role = 'crewmate' ORDER BY a.seq DESC LIMIT 1)
         FROM task_links AS l JOIN tasks AS t ON t.id = l.blocked
         WHERE l.blocker = ?1 ORDER BY t.seq",
    )?;
    Ok(stmt
        .query_map(params![blocker], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Whether `to` is reachable from `from` through "blocks" links: linking `to` → `from` would then
/// close a cycle.
pub fn reaches(conn: &Connection, from: TaskId, to: TaskId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "WITH RECURSIVE reach (id) AS (
             SELECT ?1
             UNION
             SELECT l.blocked FROM task_links AS l JOIN reach AS r ON l.blocker = r.id
         )
         SELECT EXISTS (SELECT 1 FROM reach WHERE id = ?2)",
        params![from, to],
        |r| r.get(0),
    )?)
}

/// Records that `blocker` blocks `blocked`; `false` if the link was already there.
pub fn insert_link(
    conn: &Connection,
    blocker: TaskId,
    blocked: TaskId,
    at: Timestamp,
) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "INSERT INTO task_links (blocker, blocked, created_at, seq) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (blocker, blocked) DO NOTHING",
        params![blocker, blocked, to_millis(at), seq::next(at)],
    )? == 1)
}

/// The task's own events (`task.*`), oldest first: its history.
pub fn history(conn: &Connection, task: TaskId) -> Result<Vec<EventRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, at, kind, agent_id, task_id, turn_id, summary, payload FROM events
         WHERE task_id = ?1 AND kind LIKE 'task.%'
         ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![task], |r| {
            Ok((
                r.get::<_, EventId>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<AgentId>>(3)?,
                r.get::<_, Option<TaskId>>(4)?,
                r.get::<_, Option<qm_types::ids::TurnId>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, at, kind, agent, task, turn, summary, payload)| {
            Ok(EventRow {
                id,
                at: from_millis("events.at", at)?,
                kind,
                agent,
                task,
                turn,
                summary,
                payload: json_value("events.payload", &payload)?,
            })
        })
        .collect()
}

/// The newest event of `kind` for `task`, if any.
pub fn last_event(
    conn: &Connection,
    task: TaskId,
    kind: &str,
) -> Result<Option<(EventId, serde_json::Value)>, StoreError> {
    let row = conn
        .query_row(
            "SELECT id, payload FROM events WHERE task_id = ?1 AND kind = ?2
             ORDER BY seq DESC LIMIT 1",
            params![task, kind],
            |r| Ok((r.get::<_, EventId>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(id, payload)| Ok((id, json_value("events.payload", &payload)?)))
        .transpose()
}

#[cfg(test)]
mod branch_tests {
    use super::*;
    use crate::{connection, schema, seat};
    use qm_types::domain::DeliveryMode;

    /// A migrated in-memory database with a project and the Quartermaster.
    fn db() -> (Connection, ProjectId, AgentId) {
        let mut conn = connection::open_memory().expect("open");
        schema::migrate(&mut conn).expect("migrate");
        let at = Timestamp::now();
        let dir = std::env::temp_dir();
        let qm = seat::ensure_quartermaster(&conn, &dir, at)
            .expect("quartermaster")
            .id;
        conn.execute(
            "INSERT INTO projects (name, path, origin, default_branch, delivery_mode,
                 merge_method, created_at, seq)
             VALUES ('p', '/tmp/p', 'linked', 'main', 'direct_pr', 'merge', 0, ?1)",
            params![seq::next(at)],
        )
        .expect("project");
        let project = ProjectId::new(conn.last_insert_rowid());
        (conn, project, qm)
    }

    fn new_task(project: ProjectId, qm: AgentId, branch: Option<&str>) -> NewTask {
        NewTask {
            project,
            supervisor: qm,
            kind: TaskKind::Build,
            title: "fix(upload): retry on slow tmp".to_owned(),
            goal: "g".to_owned(),
            user_words: String::new(),
            user_words_message: None,
            instructions: serde_json::json!([]),
            constraints: serde_json::json!([]),
            tests_required: false,
            builds_on: None,
            parent: None,
            retry_of: None,
            escalations: 0,
            later_steps: serde_json::json!([]),
            secondmate_request: None,
            delivery: DeliveryMode::DirectPr,
            verification: None,
            base_branch: None,
            base_ref: None,
            base_commit: None,
            branch: branch.map(str::to_owned),
            port_base: None,
            at: Timestamp::now(),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn the_first_name_no_task_holds_is_picked_closed_tasks_included() {
        let (conn, project, qm) = db();
        let all = names(&["fix/x", "fix/x-2", "fix/x-3"]);
        assert_eq!(
            first_free_branch(&conn, &all).expect("pick").as_deref(),
            Some("fix/x")
        );
        let first = insert(&conn, &new_task(project, qm, Some("fix/x"))).expect("insert");
        conn.execute(
            "UPDATE tasks SET status = 'closed', outcome = 'landed', closed_at = 0 WHERE id = ?1",
            params![first],
        )
        .expect("close");
        assert_eq!(
            first_free_branch(&conn, &all).expect("pick").as_deref(),
            Some("fix/x-2"),
            "a closed task's name still counts as taken"
        );
        insert(&conn, &new_task(project, qm, Some("fix/x-2"))).expect("insert");
        insert(&conn, &new_task(project, qm, Some("fix/x-3"))).expect("insert");
        assert_eq!(first_free_branch(&conn, &all).expect("pick"), None);
        let found = with_branch(&conn, "fix/x").expect("read").expect("a task");
        assert_eq!(found.id, first);
        assert_eq!(found.branch, "fix/x");
    }

    #[test]
    fn a_folder_task_records_no_branch() {
        let (conn, project, qm) = db();
        let mut new = new_task(project, qm, Some("fix/x"));
        new.delivery = DeliveryMode::Folder;
        let task = insert(&conn, &new).expect("insert");
        let row = load(&conn, task).expect("read").expect("row");
        assert_eq!(row.branch, "");
        assert!(with_branch(&conn, "fix/x").expect("read").is_none());
    }

    #[test]
    fn a_branch_is_renamed_only_from_its_recorded_name() {
        let (conn, project, qm) = db();
        let task = insert(&conn, &new_task(project, qm, Some("fix/x"))).expect("insert");
        assert!(!set_branch(&conn, task, "fix/other", "fix/x-2").expect("update"));
        assert!(set_branch(&conn, task, "fix/x", "fix/x-2").expect("update"));
        let row = load(&conn, task).expect("read").expect("row");
        assert_eq!(row.branch, "fix/x-2");
    }
}
