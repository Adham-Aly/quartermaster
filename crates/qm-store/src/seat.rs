//! The Quartermaster's agent row.
//!
//! There is one active Quartermaster (`agents_one_quartermaster`). Its row keeps what memory across
//! relaunches needs: the engine, account, model and effort of its session and the engine's session
//! reference, so the next launch resumes the same conversation. Other agents' rows live in
//! `agents.rs`; this module touches only the Quartermaster's.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::to_millis;
use jiff::Timestamp;
use qm_types::domain::{AgentStatus, Effort, EngineKind};
use qm_types::ids::space;
use qm_types::ids::{AccountId, AgentId, ModelId, SessionRef};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// The Quartermaster's row, as the seat reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatAgent {
    pub id: AgentId,
    pub engine: Option<EngineKind>,
    pub account: Option<AccountId>,
    pub model: Option<ModelId>,
    /// The level the stored session started with; a resumed session keeps it.
    pub effort: Option<Effort>,
    pub home: PathBuf,
    pub session_ref: Option<SessionRef>,
}

const COLUMNS: &str = "id, engine, account_id, model, effort, home, session_ref";

fn read(conn: &Connection) -> Result<Option<SeatAgent>, StoreError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM agents
                 WHERE role = 'quartermaster' AND retired_at IS NULL"
            ),
            [],
            |r| {
                Ok(SeatAgent {
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

/// The active Quartermaster row, created on first run with `home` as its working folder.
pub fn ensure_quartermaster(
    conn: &Connection,
    home: &Path,
    at: Timestamp,
) -> Result<SeatAgent, StoreError> {
    if let Some(agent) = read(conn)? {
        return Ok(agent);
    }
    conn.execute(
        "INSERT INTO agents (role, home, status, created_at, seq, id)
         VALUES ('quartermaster', ?1, ?2, ?3, ?4, ?5)",
        params![
            home.to_string_lossy(),
            AgentStatus::Idle,
            to_millis(at),
            seq::next(at),
            ids::fresh(conn, "agents", space::ROW)?,
        ],
    )?;
    read(conn)?.ok_or_else(|| StoreError::Invalid {
        column: "agents.role",
        message: "the Quartermaster's row was not found after it was created".to_owned(),
    })
}

/// The active Quartermaster row, if there is one.
pub fn quartermaster(conn: &Connection) -> Result<Option<SeatAgent>, StoreError> {
    read(conn)
}

/// Records the engine, account, model and effort of the session just opened.
pub fn update_session(
    conn: &Connection,
    agent: AgentId,
    engine: EngineKind,
    account: AccountId,
    model: &ModelId,
    effort: Effort,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET engine = ?2, account_id = ?3, model = ?4, effort = ?5 WHERE id = ?1",
        params![agent, engine, account, model, effort],
    )?;
    Ok(())
}

/// Records the model after a switch within the same engine (`SetModel`).
pub fn set_model(conn: &Connection, agent: AgentId, model: &ModelId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET model = ?2 WHERE id = ?1",
        params![agent, model],
    )?;
    Ok(())
}

/// Stores the engine's session reference (from `SessionReady`), or clears it when a new session
/// replaces the stored one, so a later launch never resumes a session that was left behind.
pub fn set_session_ref(
    conn: &Connection,
    agent: AgentId,
    session: Option<&SessionRef>,
    at: Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET session_ref = ?2, last_sign_of_life_at = ?3 WHERE id = ?1",
        params![agent, session, to_millis(at)],
    )?;
    Ok(())
}
