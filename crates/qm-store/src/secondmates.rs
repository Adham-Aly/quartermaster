//! Secondmates and the requests routed to them.
//!
//! A secondmate is one `secondmates` row, its `secondmate_projects` rows (empty: words only) and
//! one `agents` row of role `secondmate` supervised by the Quartermaster. Its standing base branch
//! is set only on your turns; the core checks that before calling the setters here.
//!
//! A routed request is one `secondmate_requests` row whose ID **is** the correlation ID. This
//! module inserts and reads requests; their status (`open → answered`) belongs to the parent
//! channel, which reads them through its own queries.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::data_dir::DataDir;
use qm_types::domain::{Effort, EngineKind, RequestStatus, TaskKind};
use qm_types::ids::space;
use qm_types::ids::{
    AccountId, AgentId, CorrelationId, ModelId, ProjectId, SecondmateId, SessionRef, TaskId, TurnId,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How many open or answered requests [`open_requests`] returns.
pub const OPEN_REQUESTS: usize = 12;

/// A secondmate, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondmateRecord {
    pub id: SecondmateId,
    /// Its agent row.
    pub agent: AgentId,
    /// The agent that supervises it: the Quartermaster.
    pub parent: AgentId,
    pub name: String,
    pub scope: String,
    /// The projects of its open crewmates, kept in step by the app.
    pub projects: Vec<(ProjectId, String)>,
    pub standing_branch: Option<String>,
    pub created_at: Timestamp,
}

/// A secondmate to create (the approved proposal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSecondmate<'a> {
    pub name: &'a str,
    pub scope: &'a str,
    pub projects: &'a [ProjectId],
    pub standing_branch: Option<&'a str>,
    /// Its supervisor: the Quartermaster's agent.
    pub parent: AgentId,
    /// Where its folders are (`secondmates/SM-02/home`), known only once its ID is.
    pub data_dir: &'a DataDir,
    pub at: Timestamp,
}

/// Writes the secondmate, its projects and its agent row (`idle`), in the caller's transaction.
pub fn insert(conn: &Connection, new: &NewSecondmate<'_>) -> Result<SecondmateRecord, StoreError> {
    let id: SecondmateId = conn.query_row(
        "INSERT INTO secondmates (name, scope, standing_base_branch, created_at, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         RETURNING id",
        params![
            new.name,
            new.scope,
            new.standing_branch,
            to_millis(new.at),
            seq::next(new.at),
            ids::fresh(conn, "secondmates", space::SECONDMATE)?,
        ],
        |r| r.get(0),
    )?;
    set_projects(conn, id, new.projects)?;
    let home = new.data_dir.secondmate(id).home;
    conn.execute(
        "INSERT INTO agents (role, supervisor_id, secondmate_id, home, status, created_at, seq, id)
         VALUES ('secondmate', ?1, ?2, ?3, 'idle', ?4, ?5, ?6)",
        params![
            new.parent,
            id,
            home.to_string_lossy(),
            to_millis(new.at),
            seq::next(new.at),
            ids::fresh(conn, "agents", space::ROW)?,
        ],
    )?;
    load(conn, id)?.ok_or_else(|| StoreError::Invalid {
        column: "secondmates.id",
        message: format!("{id} vanished after it was written"),
    })
}

const SELECT: &str = "SELECT s.id, a.id, a.supervisor_id, s.name, s.scope, s.standing_base_branch,
            s.created_at
     FROM secondmates s JOIN agents a ON a.secondmate_id = s.id AND a.role = 'secondmate'";

type Head = (
    SecondmateId,
    AgentId,
    Option<AgentId>,
    String,
    String,
    Option<String>,
    i64,
);

fn head(r: &rusqlite::Row<'_>) -> rusqlite::Result<Head> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn record(conn: &Connection, h: Head) -> Result<SecondmateRecord, StoreError> {
    let (id, agent, parent, name, scope, standing_branch, created_at) = h;
    let parent = parent.ok_or_else(|| StoreError::Invalid {
        column: "agents.supervisor_id",
        message: format!("{id} has no supervisor"),
    })?;
    Ok(SecondmateRecord {
        id,
        agent,
        parent,
        name,
        scope,
        projects: projects_of(conn, id)?,
        standing_branch,
        created_at: from_millis("secondmates.created_at", created_at)?,
    })
}

/// The projects in a secondmate's list that aren't removed, oldest first.
pub fn projects_of(
    conn: &Connection,
    id: SecondmateId,
) -> Result<Vec<(ProjectId, String)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name FROM secondmate_projects sp JOIN projects p ON p.id = sp.project_id
         WHERE sp.secondmate_id = ?1 AND p.removed_at IS NULL
         ORDER BY p.seq",
    )?;
    let rows = stmt
        .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// An active (not retired) secondmate.
pub fn load(conn: &Connection, id: SecondmateId) -> Result<Option<SecondmateRecord>, StoreError> {
    let h = conn
        .query_row(
            &format!("{SELECT} WHERE s.id = ?1 AND s.retired_at IS NULL"),
            params![id],
            head,
        )
        .optional()?;
    h.map(|h| record(conn, h)).transpose()
}

/// The active secondmate whose agent row is `agent`.
pub fn by_agent(conn: &Connection, agent: AgentId) -> Result<Option<SecondmateRecord>, StoreError> {
    let h = conn
        .query_row(
            &format!("{SELECT} WHERE a.id = ?1 AND s.retired_at IS NULL"),
            params![agent],
            head,
        )
        .optional()?;
    h.map(|h| record(conn, h)).transpose()
}

/// A secondmate's agent row.
pub fn agent_of(conn: &Connection, id: SecondmateId) -> Result<Option<AgentId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM agents WHERE secondmate_id = ?1 AND role = 'secondmate'
             ORDER BY seq DESC LIMIT 1",
            params![id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Every active secondmate, oldest first.
pub fn active(conn: &Connection) -> Result<Vec<SecondmateRecord>, StoreError> {
    let heads = {
        let mut stmt = conn.prepare(&format!(
            "{SELECT} WHERE s.retired_at IS NULL ORDER BY s.seq"
        ))?;
        stmt.query_map([], head)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    heads.into_iter().map(|h| record(conn, h)).collect()
}

/// The active secondmate called `name` (any case), if any.
pub fn by_name(conn: &Connection, name: &str) -> Result<Option<SecondmateId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM secondmates WHERE retired_at IS NULL AND name = ?1 COLLATE NOCASE
             ORDER BY seq LIMIT 1",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether an active secondmate is called `name` (any case).
pub fn name_taken(conn: &Connection, name: &str) -> Result<bool, StoreError> {
    Ok(by_name(conn, name)?.is_some())
}

/// Retires secondmate `id` and its agent row, in the caller's transaction. Nothing is deleted:
/// its history, requests and tasks stay, and its ID is never handed out again (rows are never
/// removed, so SQLite's next ID is always higher). Its pending schedules are cancelled, since
/// its loop is stopped and nothing would ever fire them. Returns whether it was active.
pub fn retire(conn: &Connection, id: SecondmateId, at: Timestamp) -> Result<bool, StoreError> {
    let n = conn.execute(
        "UPDATE secondmates SET retired_at = ?2 WHERE id = ?1 AND retired_at IS NULL",
        params![id, to_millis(at)],
    )?;
    conn.execute(
        "UPDATE schedules SET status = 'cancelled'
         WHERE status = 'pending'
           AND owner_agent_id IN (SELECT id FROM agents WHERE secondmate_id = ?1)",
        params![id],
    )?;
    conn.execute(
        "UPDATE agents SET status = 'retired', retired_at = COALESCE(retired_at, ?2)
         WHERE secondmate_id = ?1 AND role = 'secondmate' AND retired_at IS NULL",
        params![id, to_millis(at)],
    )?;
    Ok(n == 1)
}

pub fn set_scope(conn: &Connection, id: SecondmateId, scope: &str) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE secondmates SET scope = ?2 WHERE id = ?1",
        params![id, scope],
    )?;
    Ok(())
}

/// Replaces the project list.
pub fn set_projects(
    conn: &Connection,
    id: SecondmateId,
    projects: &[ProjectId],
) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM secondmate_projects WHERE secondmate_id = ?1",
        params![id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO secondmate_projects (secondmate_id, project_id) VALUES (?1, ?2)",
    )?;
    for project in projects {
        stmt.execute(params![id, project])?;
    }
    Ok(())
}

/// Makes a secondmate's project list the projects of its open crewmates, changing nothing when it
/// already is. Called in the transaction that starts, relabels or closes one of its crewmates.
pub fn sync_projects(conn: &Connection, id: SecondmateId) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.project_id FROM tasks t JOIN agents a ON a.task_id = t.id
         WHERE a.secondmate_id = ?1 AND a.role = 'crewmate' AND t.status <> 'closed'
         ORDER BY t.project_id",
    )?;
    let wanted = stmt
        .query_map(params![id], |r| r.get::<_, ProjectId>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt = conn.prepare(
        "SELECT project_id FROM secondmate_projects WHERE secondmate_id = ?1 ORDER BY project_id",
    )?;
    let have = stmt
        .query_map(params![id], |r| r.get::<_, ProjectId>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if have != wanted {
        set_projects(conn, id, &wanted)?;
    }
    Ok(())
}

/// Sets or clears the standing base branch (the core checked it came from your turn).
pub fn set_standing_branch(
    conn: &Connection,
    id: SecondmateId,
    branch: Option<&str>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE secondmates SET standing_base_branch = ?2 WHERE id = ?1",
        params![id, branch],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The secondmate's session, on its own agent row
// ---------------------------------------------------------------------------------------------

/// A secondmate's agent row, as its session needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondmateAgent {
    pub id: AgentId,
    pub engine: Option<EngineKind>,
    pub account: Option<AccountId>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub home: PathBuf,
    pub session_ref: Option<SessionRef>,
}

/// The agent row of secondmate `id`.
pub fn session_row(
    conn: &Connection,
    id: SecondmateId,
) -> Result<Option<SecondmateAgent>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id, engine, account_id, model, effort, home, session_ref FROM agents
             WHERE secondmate_id = ?1 AND role = 'secondmate' ORDER BY seq DESC LIMIT 1",
            params![id],
            |r| {
                Ok(SecondmateAgent {
                    id: r.get(0)?,
                    engine: r.get(1)?,
                    account: r.get(2)?,
                    model: r.get(3)?,
                    effort: r.get(4)?,
                    home: PathBuf::from(r.get::<_, String>(5)?),
                    session_ref: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// Records what its session runs on (and where its kept chat starts,
/// `agents::CHAT_KEPT_FROM`).
pub fn update_session(
    conn: &Connection,
    agent: AgentId,
    engine: EngineKind,
    account: AccountId,
    model: &ModelId,
    effort: Effort,
) -> Result<(), StoreError> {
    conn.execute(
        &format!(
            "UPDATE agents SET engine = ?2, account_id = ?3, model = ?4, effort = ?5, {}
             WHERE id = ?1 AND role = 'secondmate'",
            crate::agents::CHAT_KEPT_FROM
        ),
        params![
            agent,
            engine,
            account,
            model,
            effort,
            crate::agents::chat_start()
        ],
    )?;
    Ok(())
}

/// Stores (or forgets) its session reference.
pub fn set_session_ref(
    conn: &Connection,
    agent: AgentId,
    session: Option<&SessionRef>,
    at: Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET session_ref = ?2, last_sign_of_life_at = ?3
         WHERE id = ?1 AND role = 'secondmate'",
        params![agent, session, to_millis(at)],
    )?;
    Ok(())
}

/// A model change within the same engine and account.
pub fn set_model(conn: &Connection, agent: AgentId, model: &ModelId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET model = ?2 WHERE id = ?1 AND role = 'secondmate'",
        params![agent, model],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// A later step the request carries (the shape of `qm_tools::defs::LaterStep`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredLaterStep {
    pub kind: TaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

/// The overrides a request carries, accepted on the routing turn (`secondmate_requests.overrides`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later_steps: Vec<StoredLaterStep>,
}

impl RequestOverrides {
    /// Whether the request names any override.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.effort.is_none()
            && !self.premium
            && self.later_steps.is_empty()
    }
}

/// A request to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRequest<'a> {
    pub secondmate: SecondmateId,
    /// The routing turn.
    pub turn: TurnId,
    pub request: &'a str,
    /// Your words, copied from your message on a turn you started; empty otherwise.
    pub user_words: &'a str,
    pub instructions: &'a [String],
    pub base_branch: Option<&'a str>,
    pub overrides: &'a RequestOverrides,
    pub at: Timestamp,
}

/// A request, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    pub id: CorrelationId,
    pub secondmate: SecondmateId,
    pub turn: TurnId,
    pub request: String,
    pub user_words: String,
    pub instructions: Vec<String>,
    pub base_branch: Option<String>,
    pub overrides: RequestOverrides,
    pub status: RequestStatus,
    pub created_at: Timestamp,
}

/// Writes an `open` request; its ID is the correlation ID.
pub fn insert_request(
    conn: &Connection,
    new: &NewRequest<'_>,
) -> Result<CorrelationId, StoreError> {
    let instructions =
        serde_json::to_string(new.instructions).map_err(|source| StoreError::Json {
            column: "secondmate_requests.instructions",
            source,
        })?;
    let overrides = serde_json::to_string(new.overrides).map_err(|source| StoreError::Json {
        column: "secondmate_requests.overrides",
        source,
    })?;
    Ok(conn.query_row(
        "INSERT INTO secondmate_requests
             (secondmate_id, turn_id, request, user_words, instructions, base_branch, overrides,
              status, created_at, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8, ?9, ?10)
         RETURNING id",
        params![
            new.secondmate,
            new.turn,
            new.request,
            new.user_words,
            instructions,
            new.base_branch,
            overrides,
            to_millis(new.at),
            seq::next(new.at),
            ids::fresh(conn, "secondmate_requests", space::CORRELATION)?,
        ],
        |r| r.get(0),
    )?)
}

const REQUEST_COLUMNS: &str = "id, secondmate_id, turn_id, request, user_words, instructions, \
     base_branch, overrides, status, created_at";

type RawRequest = (
    CorrelationId,
    SecondmateId,
    TurnId,
    String,
    String,
    String,
    Option<String>,
    String,
    RequestStatus,
    i64,
);

fn raw_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRequest> {
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
        r.get(9)?,
    ))
}

fn request_row(raw: RawRequest) -> Result<RequestRow, StoreError> {
    let (
        id,
        secondmate,
        turn,
        request,
        user_words,
        instructions,
        base_branch,
        overrides,
        status,
        at,
    ) = raw;
    Ok(RequestRow {
        id,
        secondmate,
        turn,
        request,
        user_words,
        instructions: serde_json::from_str(&instructions).map_err(|source| StoreError::Json {
            column: "secondmate_requests.instructions",
            source,
        })?,
        base_branch,
        overrides: serde_json::from_str(&overrides).map_err(|source| StoreError::Json {
            column: "secondmate_requests.overrides",
            source,
        })?,
        status,
        created_at: from_millis("secondmate_requests.created_at", at)?,
    })
}

pub fn load_request(
    conn: &Connection,
    id: CorrelationId,
) -> Result<Option<RequestRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {REQUEST_COLUMNS} FROM secondmate_requests WHERE id = ?1"),
            params![id],
            raw_request,
        )
        .optional()?;
    raw.map(request_row).transpose()
}

/// A secondmate's open and answered requests, newest first, at most [`OPEN_REQUESTS`].
pub fn open_requests(conn: &Connection, id: SecondmateId) -> Result<Vec<RequestRow>, StoreError> {
    let raws = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {REQUEST_COLUMNS} FROM secondmate_requests
             WHERE secondmate_id = ?1 AND status IN ('open', 'answered')
             ORDER BY seq DESC LIMIT ?2"
        ))?;
        stmt.query_map(
            params![id, i64::try_from(OPEN_REQUESTS).unwrap_or(i64::MAX)],
            raw_request,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    raws.into_iter().map(request_row).collect()
}

/// Who started a turn: the message it names when you started it (for a request's overrides,
/// applied with the routing turn's authority).
pub fn turn_message(
    conn: &Connection,
    turn: TurnId,
) -> Result<Option<qm_types::ids::MessageId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT message_id FROM turns WHERE id = ?1 AND started_by = 'user'",
            params![turn],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// Tasks supervised by `agent` (a secondmate's crewmates' tasks), open or not.
pub fn tasks_of(conn: &Connection, agent: AgentId) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE supervisor_id = ?1 ORDER BY seq")?;
    let rows = stmt
        .query_map(params![agent], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `tasks_of`'s open subset, with each task's project, as `(task, project, status word)`.
pub fn open_tasks_of(
    conn: &Connection,
    agent: AgentId,
) -> Result<Vec<(TaskId, ProjectId, String)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, status FROM tasks
         WHERE supervisor_id = ?1 AND status <> 'closed' ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![agent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
