//! A worker's session as its screen shows it.
//!
//! `session_lines` is append-only (a trigger refuses updates): the recorder writes the worker's
//! committed text blocks, its tool lines, the brief and wakes it was given, and direct
//! intervention writes your message. [`header`] reads the facts the screen's header and status
//! line are built from.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{AgentStatus, Effort, Role, TaskStatus};
use qm_types::ids::space;
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
        "INSERT INTO session_lines (agent_id, task_id, turn_id, kind, text, detail, message_id, created_at, seq,
                                    id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            ids::fresh(conn, "session_lines", space::ROW)?,
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
    /// The crewmate's effort, else the one its routing chose.
    pub effort: Option<Effort>,
    /// The task's branch and the branch it goes back to; absent for a folder task.
    pub branch: Option<(String, String)>,
    /// The project's folder, where a folder task works.
    pub folder: String,
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
                    a.model, a.status, s.role, s.secondmate_id,
                    COALESCE(a.effort, (SELECT d.effort FROM routing_decisions AS d
                                        WHERE d.task_id = t.id ORDER BY d.seq DESC LIMIT 1)),
                    t.branch, COALESCE(t.base_branch, p.default_branch), p.path
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
                    r.get::<_, Option<Effort>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, String>(12)?,
                ))
            },
        )
        .optional()?;
    let Some((
        agent,
        title,
        status,
        since,
        project,
        model,
        agent_status,
        role,
        secondmate,
        effort,
        branch,
        target,
        folder,
    )) = raw
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
        effort,
        branch: branch.map(|b| (b, target)),
        folder,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_says_where_a_crewmate_works_and_at_what_effort() {
        let mut conn = Connection::open_in_memory().expect("open fixture");
        crate::schema::migrate(&mut conn).expect("migrate");
        conn.execute_batch(
            "INSERT INTO agents (id, role, home, status, created_at)
             VALUES (1, 'quartermaster', '/fixture/seat', 'idle', 0);
             INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                                   merge_method, created_at, seq)
             VALUES (1, 'notes', '/fixture/notes', 'workspace', '', 'folder', 'squash', 0, 1),
                    (2, 'app', '/fixture/app', 'linked', 'main', 'branch_only', 'squash', 0, 2);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                                delivery_mode, branch, base_branch, status, created_at, seq)
             VALUES (1, 1, 1, 'fix', 'Folder task', 'g', 'folder', NULL, NULL, 'running', 0, 1),
                    (2, 2, 1, 'fix', 'Git task', 'g', 'branch_only', 'qm/task-0002', NULL,
                     'running', 1, 2),
                    (3, 2, 1, 'fix', 'Typed base', 'g', 'branch_only', 'qm/task-0003', 'dev',
                     'running', 2, 3);
             INSERT INTO agents (id, role, supervisor_id, task_id, model, effort, home, status,
                                 created_at, seq)
             VALUES (11, 'crewmate', 1, 1, 'claude:sonnet-5-5', 'high', '/f/1', 'idle', 0, 2),
                    (12, 'crewmate', 1, 2, 'claude:sonnet-5-5', NULL, '/f/2', 'idle', 0, 3),
                    (13, 'crewmate', 1, 3, NULL, NULL, '/f/3', 'starting', 0, 4);
             INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq)
             VALUES (1, 'claude', 'Claude Max', 'subscription', 'claude', 0, 1);
             INSERT INTO routing_decisions (task_id, facts, rule, decided_by, tier_landed,
                                            tier_used, model, account_id, effort, why,
                                            created_at, seq)
             VALUES (2, '{}', 'r', 'rules', 'medium', 'medium', 'claude:sonnet-5-5', 1,
                     'low', 'w', 0, 1);",
        )
        .expect("seed tasks");
        let task = |n| TaskId::new(n).expect("task id");
        let folder = header(&conn, task(1)).unwrap().unwrap();
        assert_eq!(
            (folder.branch, folder.folder.as_str(), folder.effort),
            (None, "/fixture/notes", Some(Effort::High))
        );
        // No effort of its own: the one its routing chose.
        let git = header(&conn, task(2)).unwrap().unwrap();
        assert_eq!(
            (git.branch, git.effort),
            (
                Some(("qm/task-0002".to_owned(), "main".to_owned())),
                Some(Effort::Low)
            )
        );
        let typed = header(&conn, task(3)).unwrap().unwrap();
        assert_eq!(
            (typed.branch, typed.effort),
            (Some(("qm/task-0003".to_owned(), "dev".to_owned())), None)
        );
    }
}
