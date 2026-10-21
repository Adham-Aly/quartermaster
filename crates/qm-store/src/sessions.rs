//! A worker's session as its screen shows it.
//!
//! `session_lines` is append-only (a trigger refuses updates): the recorder writes the worker's
//! committed text blocks, its tool lines, the brief and wakes it was given, and direct
//! intervention writes your message. [`header`] reads the facts the screen's header and status
//! line are built from.

use crate::StoreError;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{AgentStatus, Role, TaskStatus};
use qm_types::ids::{AgentId, MessageId, ModelId, SecondmateId, TaskId, TurnId};
use rusqlite::{Connection, OptionalExtension, params};

/// A row's text is cut to this many characters.
pub const MAX_LINE_CHARS: usize = 8_000;

/// Who a session line is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    Supervisor,
    App,
    You,
    Agent,
    Tool,
}

impl LineKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supervisor => "supervisor",
            Self::App => "app",
            Self::You => "you",
            Self::Agent => "agent",
            Self::Tool => "tool",
        }
    }

    fn parse(s: &str) -> Result<Self, StoreError> {
        Ok(match s {
            "supervisor" => Self::Supervisor,
            "app" => Self::App,
            "you" => Self::You,
            "agent" => Self::Agent,
            "tool" => Self::Tool,
            other => {
                return Err(StoreError::Invalid {
                    column: "session_lines.kind",
                    message: format!("unknown kind {other:?}"),
                });
            }
        })
    }
}

/// A line to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSessionLine {
    pub agent: AgentId,
    pub task: Option<TaskId>,
    pub turn: Option<TurnId>,
    pub kind: LineKind,
    pub text: String,
    pub detail: Option<String>,
    /// Your message, for a `you` line.
    pub message: Option<MessageId>,
    pub at: Timestamp,
}

impl NewSessionLine {
    pub fn new(agent: AgentId, kind: LineKind, text: impl Into<String>, at: Timestamp) -> Self {
        Self {
            agent,
            task: None,
            turn: None,
            kind,
            text: text.into(),
            detail: None,
            message: None,
            at,
        }
    }
}

/// A stored line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLineRow {
    pub id: i64,
    pub agent: AgentId,
    pub task: Option<TaskId>,
    pub turn: Option<TurnId>,
    pub kind: LineKind,
    pub text: String,
    pub detail: Option<String>,
    pub message: Option<MessageId>,
    pub at: Timestamp,
}

fn capped(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(MAX_LINE_CHARS - 1).collect();
        out.push('…');
        out
    }
}

/// Appends a line; returns its row ID.
pub fn insert(conn: &Connection, line: &NewSessionLine) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO session_lines (agent_id, task_id, turn_id, kind, text, detail, message_id, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            line.agent,
            line.task,
            line.turn,
            line.kind.as_str(),
            capped(&line.text),
            line.detail.as_deref().map(capped),
            line.message,
            to_millis(line.at),
            seq::next(line.at),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Appends a line for your message unless one already shows it (direct intervention writes it
/// at once; the recorder sees the same message again when the runner delivers it). Returns
/// whether it wrote one.
pub fn insert_message_once(conn: &Connection, line: &NewSessionLine) -> Result<bool, StoreError> {
    if let Some(message) = line.message
        && has_message(conn, message)?
    {
        return Ok(false);
    }
    insert(conn, line)?;
    Ok(true)
}

/// Whether a line already shows `message`.
pub fn has_message(conn: &Connection, message: MessageId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM session_lines WHERE message_id = ?1)",
        params![message],
        |r| r.get(0),
    )?)
}

/// The newest `limit` lines of `agent`'s session, oldest first.
pub fn recent(
    conn: &Connection,
    agent: AgentId,
    limit: usize,
) -> Result<Vec<SessionLineRow>, StoreError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, task_id, turn_id, kind, text, detail, message_id, created_at
         FROM (SELECT * FROM session_lines WHERE agent_id = ?1 ORDER BY seq DESC LIMIT ?2)
         ORDER BY seq",
    )?;
    let raw = stmt
        .query_map(params![agent, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, AgentId>(1)?,
                r.get::<_, Option<TaskId>>(2)?,
                r.get::<_, Option<TurnId>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<MessageId>>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(|(id, agent, task, turn, kind, text, detail, message, at)| {
            Ok(SessionLineRow {
                id,
                agent,
                task,
                turn,
                kind: LineKind::parse(&kind)?,
                text,
                detail,
                message,
                at: from_millis("session_lines.created_at", at)?,
            })
        })
        .collect()
}

/// What a crewmate's session header and status line are built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHeader {
    pub agent: AgentId,
    pub task: TaskId,
    pub title: String,
    pub status: TaskStatus,
    /// When the task started (or was created, before it started).
    pub since: Timestamp,
    pub project: String,
    pub model: Option<ModelId>,
    pub agent_status: AgentStatus,
    /// The supervisor's role, and its secondmate ID when it is one.
    pub supervisor: (Role, Option<SecondmateId>),
    /// Labels of the crewmate's jobs still running, oldest first ("J3").
    pub jobs: Vec<String>,
}

/// The header facts of `task`'s crewmate (its newest), or `None` if the task has none.
pub fn header(conn: &Connection, task: TaskId) -> Result<Option<SessionHeader>, StoreError> {
    let raw = conn
        .query_row(
            "SELECT a.id, t.title, t.status, COALESCE(t.started_at, t.created_at), p.name,
                    a.model, a.status, s.role, s.secondmate_id
             FROM tasks AS t
             JOIN agents AS a ON a.task_id = t.id AND a.role = 'crewmate'
             JOIN projects AS p ON p.id = t.project_id
             JOIN agents AS s ON s.id = t.supervisor_id
             WHERE t.id = ?1
             ORDER BY a.seq DESC LIMIT 1",
            params![task],
            |r| {
                Ok((
                    r.get::<_, AgentId>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, TaskStatus>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<ModelId>>(5)?,
                    r.get::<_, AgentStatus>(6)?,
                    r.get::<_, Role>(7)?,
                    r.get::<_, Option<SecondmateId>>(8)?,
                ))
            },
        )
        .optional()?;
    let Some((agent, title, status, since, project, model, agent_status, role, secondmate)) = raw
    else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare("SELECT label FROM jobs WHERE agent_id = ?1 AND exited_at IS NULL ORDER BY seq")?;
    let jobs = stmt
        .query_map(params![agent], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(SessionHeader {
        agent,
        task,
        title,
        status,
        since: from_millis("tasks.started_at", since)?,
        project,
        model,
        agent_status,
        supervisor: (role, secondmate),
        jobs,
    }))
}

/// A task's title, for the brief's line.
pub fn task_title(conn: &Connection, task: TaskId) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT title FROM tasks WHERE id = ?1",
            params![task],
            |r| r.get(0),
        )
        .optional()?)
}
