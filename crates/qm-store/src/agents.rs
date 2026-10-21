//! Crewmates' agent rows and their turns.
//!
//! The Quartermaster's row lives in `seat.rs`; this module keeps crewmates': created in the start
//! transaction, routed, given a session, and retired when the task closes. A crewmate's turns use
//! the same `turns` table as the seat's: [`start_crew_turn`] records one, and
//! [`finish_crew_turn`] commits everything it leaves behind in one transaction (the row, its
//! wakes consumed or released, the final text kept as a report, the `task.turn_ended` event).

use crate::StoreError;
use crate::events::{self, NewEvent};
use crate::reports::{self, NewReport};
use crate::seq;
use crate::time::to_millis;
use crate::wakes;
use jiff::Timestamp;
use qm_types::domain::{AgentStatus, Effort, EngineKind, ReportSource, StartedBy, TurnStatus};
use qm_types::ids::{
    AccountId, AgentId, ModelId, ReportId, SecondmateId, SessionRef, TaskId, TurnId,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// `events.kind` of a crewmate's finished turn.
pub const TURN_ENDED: &str = "task.turn_ended";
/// Why a crewmate turn was interrupted because its task closed.
pub const REASON_TASK_CLOSED: &str = "task_closed";

/// A crewmate to record (in the start transaction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewCrewmate<'a> {
    pub task: TaskId,
    pub supervisor: AgentId,
    /// The secondmate whose crewmate this is, if any.
    pub secondmate: Option<SecondmateId>,
    /// The worktree.
    pub home: &'a Path,
    pub at: Timestamp,
}

/// A crewmate's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrewAgent {
    pub id: AgentId,
    pub task: TaskId,
    pub supervisor: AgentId,
    pub engine: Option<EngineKind>,
    pub account: Option<AccountId>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub home: PathBuf,
    pub session_ref: Option<SessionRef>,
    pub status: AgentStatus,
}

/// Records a crewmate as `starting`.
pub fn insert_crewmate(conn: &Connection, c: &NewCrewmate<'_>) -> Result<AgentId, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO agents (role, supervisor_id, task_id, home, status, created_at, seq)
         VALUES ('crewmate', ?1, ?2, ?3, 'starting', ?4, ?5)
         RETURNING id",
        params![
            c.supervisor,
            c.task,
            c.home.to_string_lossy(),
            to_millis(c.at),
            seq::next(c.at),
        ],
        |r| r.get(0),
    )?)
}

const COLUMNS: &str =
    "id, task_id, supervisor_id, engine, account_id, model, effort, home, session_ref, status";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CrewAgent> {
    Ok(CrewAgent {
        id: r.get(0)?,
        task: r.get(1)?,
        supervisor: r.get(2)?,
        engine: r.get(3)?,
        account: r.get(4)?,
        model: r.get(5)?,
        effort: r.get(6)?,
        home: PathBuf::from(r.get::<_, String>(7)?),
        session_ref: r.get(8)?,
        status: r.get(9)?,
    })
}

/// The crewmate of `task` (the newest, should there ever be more than one).
pub fn crewmate_of(conn: &Connection, task: TaskId) -> Result<Option<CrewAgent>, StoreError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM agents WHERE task_id = ?1 AND role = 'crewmate'
                 ORDER BY seq DESC LIMIT 1"
            ),
            params![task],
            row,
        )
        .optional()?)
}

/// A crewmate by its agent row.
pub fn load(conn: &Connection, agent: AgentId) -> Result<Option<CrewAgent>, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM agents WHERE id = ?1 AND role = 'crewmate'"),
            params![agent],
            row,
        )
        .optional()?)
}

/// The role of any agent, and its secondmate if it is one.
pub fn role_of(
    conn: &Connection,
    agent: AgentId,
) -> Result<Option<(qm_types::domain::Role, Option<SecondmateId>)>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT role, secondmate_id FROM agents WHERE id = ?1",
            params![agent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Stores the routed engine, account, model and effort.
pub fn set_routed(
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

/// Sets the agent's status; `retired` also sets `retired_at`. A retired agent never changes again.
pub fn set_status(
    conn: &Connection,
    agent: AgentId,
    status: AgentStatus,
    at: Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET status = ?2,
             retired_at = CASE WHEN ?2 = 'retired' THEN COALESCE(retired_at, ?3) ELSE retired_at END
         WHERE id = ?1 AND retired_at IS NULL",
        params![agent, status, to_millis(at)],
    )?;
    Ok(())
}

/// Stores the engine's session reference (or clears it for a new session), as a sign of life.
pub fn set_session(
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

/// Records a sign of life.
pub fn touch(conn: &Connection, agent: AgentId, at: Timestamp) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET last_sign_of_life_at = ?2 WHERE id = ?1",
        params![agent, to_millis(at)],
    )?;
    Ok(())
}

/// Records a crewmate turn as running. The schema's one-running-turn index keeps it to one turn
/// at a time per agent.
pub fn start_crew_turn(
    conn: &Connection,
    agent: AgentId,
    started_by: StartedBy,
    model: Option<&ModelId>,
    at: Timestamp,
) -> Result<TurnId, StoreError> {
    let (by, message) = match started_by {
        StartedBy::User { message } => ("user", Some(message)),
        StartedBy::AppEvent => ("app_event", None),
    };
    Ok(conn.query_row(
        "INSERT INTO turns (agent_id, started_by, message_id, model, started_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         RETURNING id",
        params![agent, by, message, model, to_millis(at), seq::next(at)],
        |r| r.get(0),
    )?)
}

/// A crewmate turn's usage.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CrewUsage {
    pub model_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// Everything a finished crewmate turn leaves behind.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedCrewTurn<'a> {
    pub turn: TurnId,
    pub agent: AgentId,
    pub task: TaskId,
    /// `completed`, `interrupted` or `failed`.
    pub status: TurnStatus,
    pub reason: Option<&'a str>,
    pub usage: CrewUsage,
    pub model: Option<&'a ModelId>,
    pub final_text: &'a str,
    /// Keep `final_text` as a `final_text` report: the turn made no clear `report` call.
    pub keep_final_text: bool,
    /// Put the turn's wakes back instead of consuming them (a quit).
    pub release_wakes: bool,
    pub at: Timestamp,
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Commits a finished crewmate turn in one transaction. Returns the `final_text` report's ID when
/// one was kept.
pub fn finish_crew_turn(
    conn: &mut Connection,
    f: &FinishedCrewTurn<'_>,
) -> Result<Option<ReportId>, StoreError> {
    if f.status == TurnStatus::Running {
        return Err(StoreError::Invalid {
            column: "turns.status",
            message: "a finished turn can't be running".to_owned(),
        });
    }
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE turns SET status = ?2, outcome_reason = ?3, model = COALESCE(?4, model),
             model_calls = ?5, input_tokens = ?6, output_tokens = ?7, thinking_tokens = ?8,
             cache_read_tokens = ?9, cache_write_tokens = ?10, cost_usd = ?11, ended_at = ?12
         WHERE id = ?1 AND status = 'running'",
        params![
            f.turn,
            f.status,
            f.reason,
            f.model,
            i64::from(f.usage.model_calls),
            clamp(f.usage.input_tokens),
            clamp(f.usage.output_tokens),
            clamp(f.usage.thinking_tokens),
            clamp(f.usage.cache_read_tokens),
            clamp(f.usage.cache_write_tokens),
            f.usage.cost_usd,
            to_millis(f.at),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::Invalid {
            column: "turns.status",
            message: format!("turn {} is not running", f.turn),
        });
    }
    if f.release_wakes {
        wakes::release_unconsumed(&tx, f.turn)?;
    } else {
        wakes::consume_wakes(&tx, f.turn, f.at)?;
    }
    let report = if f.keep_final_text && !f.final_text.trim().is_empty() {
        Some(reports::insert(
            &tx,
            &NewReport {
                task: f.task,
                agent: f.agent,
                source: ReportSource::FinalText,
                final_text: Some(f.final_text),
                ..NewReport::empty(f.task, f.agent, f.at)
            },
        )?)
    } else {
        None
    };
    let summary = match f.reason {
        Some(reason) => format!("{} turn {}: {reason}", f.task, f.status),
        None => format!("{} turn {}", f.task, f.status),
    };
    events::insert(
        &tx,
        &NewEvent {
            kind: TURN_ENDED,
            agent: Some(f.agent),
            task: Some(f.task),
            turn: Some(f.turn),
            summary: &summary,
            payload: serde_json::json!({
                "status": f.status,
                "reason": f.reason,
                "final_text_kept": report.is_some(),
            }),
            at: f.at,
        },
    )?;
    tx.commit()?;
    Ok(report)
}

/// A crewmate turn startup found running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptedCrewTurn {
    pub agent: AgentId,
    pub task: TaskId,
    pub turn: TurnId,
}

/// Startup: every crewmate turn still `running` becomes `interrupted` with reason `app_stopped`,
/// and its wakes go back in the inbox. One transaction.
pub fn interrupt_running_crew_turns(
    conn: &mut Connection,
    at: Timestamp,
) -> Result<Vec<InterruptedCrewTurn>, StoreError> {
    let tx = conn.transaction()?;
    let rows = {
        let mut stmt = tx.prepare(
            "UPDATE turns SET status = 'interrupted', outcome_reason = ?1, ended_at = ?2
             WHERE status = 'running'
               AND agent_id IN (SELECT id FROM agents WHERE role = 'crewmate')
             RETURNING id, agent_id, (SELECT task_id FROM agents WHERE id = turns.agent_id)",
        )?;
        stmt.query_map(
            params![crate::turns::REASON_APP_STOPPED, to_millis(at)],
            |r| {
                Ok(InterruptedCrewTurn {
                    turn: r.get(0)?,
                    agent: r.get(1)?,
                    task: r.get(2)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for t in &rows {
        wakes::release_unconsumed(&tx, t.turn)?;
        events::insert(
            &tx,
            &NewEvent {
                kind: TURN_ENDED,
                agent: Some(t.agent),
                task: Some(t.task),
                turn: Some(t.turn),
                summary: &format!("{} turn interrupted: app_stopped", t.task),
                payload: serde_json::json!({
                    "status": "interrupted",
                    "reason": crate::turns::REASON_APP_STOPPED,
                }),
                at,
            },
        )?;
    }
    tx.commit()?;
    Ok(rows)
}

/// Startup: every crewmate of an open task that isn't retired is `stopped` (its process is gone).
pub fn stop_live_crewmates(conn: &Connection) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE agents SET status = 'stopped'
         WHERE role = 'crewmate' AND retired_at IS NULL AND status <> 'stopped'
           AND task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
        [],
    )?)
}

/// Whether the agent's last turn failed at the provider (the idle ladder skips its nudge).
pub fn last_turn_failed_at_provider(conn: &Connection, agent: AgentId) -> Result<bool, StoreError> {
    let reason: Option<Option<String>> = conn
        .query_row(
            "SELECT outcome_reason FROM turns WHERE agent_id = ?1 AND status <> 'running'
             ORDER BY seq DESC LIMIT 1",
            params![agent],
            |r| r.get(0),
        )
        .optional()?;
    Ok(reason.flatten().as_deref() == Some("provider"))
}

/// The agent's newest finished turn: its ID, status and reason.
pub fn last_finished_turn(
    conn: &Connection,
    agent: AgentId,
) -> Result<Option<(TurnId, TurnStatus, Option<String>)>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id, status, outcome_reason FROM turns WHERE agent_id = ?1 AND status <> 'running'
             ORDER BY seq DESC LIMIT 1",
            params![agent],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}
