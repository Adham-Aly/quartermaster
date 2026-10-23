//! An orchestrator's turns.
//!
//! - [`start_turn`] writes the `turns` row and its `seat.turn_started` event in one transaction.
//!   The schema holds that a turn you started names your message, and that an agent runs one turn
//!   at a time (`turns_one_running`).
//! - [`finish_turn`] commits everything a finished turn leaves behind in **one transaction**: the
//!   row's outcome, the turn's events (`seat.turn_ended`, each hidden close-out, a limit hit), and
//!   its wakes, consumed (or released after a quit, so they run again).
//! - Startup recovery: [`interrupt_running`] marks the turns the app stopped in the middle of and
//!   puts their wakes back; [`cut_short`] lists the turns a quit or a stop cut short until each is
//!   [`mark_recovered`].
//!
//! The seat's event rows are written here with a small private helper (the same columns as the
//! events module's insert), so a turn, its events and its wakes commit atomically.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::to_millis;
use crate::wakes;
use jiff::Timestamp;
use qm_types::domain::{StartedBy, TurnStatus};
use qm_types::ids::space;
use qm_types::ids::{AgentId, EventId, MessageId, ModelId, TaskId, TurnId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// The `events.kind` values written here; the same strings as `qm_core::turn_io::event_kind`.
pub mod kind {
    pub const TURN_STARTED: &str = "seat.turn_started";
    pub const TURN_ENDED: &str = "seat.turn_ended";
    pub const CLOSE_OUT: &str = "seat.close_out";
    pub const LIMIT_HIT: &str = "seat.limit_hit";
    pub const COMPACTED: &str = "seat.compacted";
    pub const END_CONVERSATION: &str = "seat.end_conversation";
    /// You ran `/clear`: the payload's `after_turn` is the last turn before it (0 for none), and
    /// a new session's summary covers only the turns after it.
    pub const CLEARED: &str = "seat.cleared";
}

/// The reason a turn the app stopped in the middle of is marked with at startup.
pub const REASON_APP_STOPPED: &str = "app_stopped";
/// The reason a turn interrupted because the app was quitting is marked with.
pub const REASON_APP_QUIT: &str = "app_quit";
/// Added to a cut-short turn's reason once startup has handled it.
pub const RECOVERED_SUFFIX: &str = "_recovered";

/// How many characters of a close-out's first line its event summary keeps.
pub const CLOSE_OUT_SUMMARY_CHARS: usize = 120;

/// A turn to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTurn {
    pub agent: AgentId,
    pub started_by: StartedBy,
    /// The model the session runs, when known.
    pub model: Option<ModelId>,
    pub at: Timestamp,
}

/// One effective action, as `turns.effective_actions` keeps it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedAction {
    pub tool: String,
    /// What it did, as the app's acknowledgement words it ("Started: T-0048"), or `None` for a
    /// line shown to you or a change with no words of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
}

/// A turn's usage, summed over its attempts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TurnTotals {
    pub model_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// A limit that stopped the turn, for `seat.limit_hit`.
#[derive(Clone, Debug, PartialEq)]
pub struct LimitRecord {
    /// `call_time`, `turn_time`, `model_calls` or `output_tokens`.
    pub limit: String,
    /// The plain reason ("a message reached the 8,192-token output cap (thinking included)").
    pub reason: String,
    /// The turn's counters when it was stopped.
    pub counters: serde_json::Value,
}

/// Everything a finished turn leaves behind.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedTurn {
    pub turn: TurnId,
    pub agent: AgentId,
    /// `completed`, `interrupted` or `failed`; never `running`.
    pub status: TurnStatus,
    /// A short code (`no_action`, `app_quit`, `limit_call_time`, …), or `None` for a plain
    /// completed turn.
    pub reason: Option<String>,
    /// The plain sentence behind a failure, for the event log.
    pub detail: Option<String>,
    pub actions: Vec<RecordedAction>,
    pub retried: bool,
    pub usage: TurnTotals,
    pub model: Option<ModelId>,
    /// Each attempt's non-empty plain final text: the hidden close-out.
    pub close_outs: Vec<String>,
    pub limit: Option<LimitRecord>,
    /// Put the turn's wakes back in the inbox instead of consuming them (a quit).
    pub release_wakes: bool,
    pub at: Timestamp,
}

/// A turn startup marked interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupted {
    pub turn: TurnId,
    /// Your message, if you started it.
    pub message: Option<MessageId>,
}

/// A turn a quit or a stop cut short, not yet handled by startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutShort {
    pub turn: TurnId,
    pub started_by: StartedBy,
}

/// An event the seat writes outside a turn's commit (`seat.compacted`, `seat.end_conversation`).
#[derive(Clone, Debug, PartialEq)]
pub struct SeatEvent {
    pub kind: &'static str,
    pub agent: AgentId,
    pub turn: Option<TurnId>,
    pub summary: String,
    pub payload: serde_json::Value,
    pub at: Timestamp,
}

fn started_by_column(s: StartedBy) -> &'static str {
    match s {
        StartedBy::User { .. } => "user",
        StartedBy::AppEvent => "app_event",
    }
}

fn json(column: &'static str, v: &impl serde::Serialize) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json { column, source })
}

/// The same columns as the events module's insert.
#[allow(clippy::too_many_arguments)]
fn insert_event(
    conn: &Connection,
    kind: &str,
    agent: AgentId,
    task: Option<TaskId>,
    turn: Option<TurnId>,
    summary: &str,
    payload: &serde_json::Value,
    at: Timestamp,
) -> Result<EventId, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO events (at, kind, agent_id, task_id, turn_id, summary, payload, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         RETURNING id",
        params![
            to_millis(at),
            kind,
            agent,
            task,
            turn,
            summary,
            json("events.payload", payload)?,
            seq::next(at),
            ids::fresh(conn, "events", space::ROW)?,
        ],
        |r| r.get(0),
    )?)
}

/// The active Quartermaster's agent row, if one exists yet.
pub fn seat_agent(conn: &Connection) -> Result<Option<AgentId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM agents WHERE role = 'quartermaster' AND retired_at IS NULL",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// The reason a wake turn gives when it found no wakes: it never reached the model.
pub const NO_WAKES: &str = "no_wakes";

/// Records a turn as running, and its `seat.turn_started` event, in one transaction.
pub fn start_turn(conn: &mut Connection, turn: &NewTurn) -> Result<TurnId, StoreError> {
    let tx = conn.transaction()?;
    let message = turn.started_by.message();
    let id: TurnId = tx.query_row(
        "INSERT INTO turns (agent_id, started_by, message_id, model, started_at, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
        params![
            turn.agent,
            started_by_column(turn.started_by),
            message,
            turn.model,
            to_millis(turn.at),
            seq::next(turn.at),
            ids::fresh(&tx, "turns", space::TURN)?,
        ],
        |r| r.get(0),
    )?;
    let summary = if message.is_some() {
        "turn started by you"
    } else {
        "turn started by an app event"
    };
    let payload = serde_json::json!({
        "started_by": started_by_column(turn.started_by),
        "message": message,
    });
    let event = insert_event(
        &tx,
        kind::TURN_STARTED,
        turn.agent,
        None,
        Some(id),
        summary,
        &payload,
        turn.at,
    )?;
    let event_seq: i64 = tx.query_row("SELECT seq FROM events WHERE id = ?1", [event], |r| {
        r.get(0)
    })?;
    shift_watermark(&tx, turn.agent, id, (event, event_seq), turn.at)?;
    tx.commit()?;
    Ok(id)
}

/// Makes `turn` the agent's newest turn for the block's since-lines (`block_watermarks`). The turn
/// it replaces moves to `prev_*`, so a `no_wakes` end can put it back ([`restore_watermark`]).
fn shift_watermark(
    tx: &Transaction<'_>,
    agent: AgentId,
    turn: TurnId,
    (event, event_seq): (EventId, i64),
    at: Timestamp,
) -> Result<(), StoreError> {
    // An UPDATE, not an upsert: with the dirty-mark triggers, SQLite 3.54 refuses an upsert's
    // DO UPDATE on a mirrored table (a UNIQUE failure in the trigger's temp table).
    let moved = tx.execute(
        "UPDATE block_watermarks
         SET prev_turn_id = turn_id, prev_event_id = event_id, prev_event_seq = event_seq,
             turn_id = ?2, event_id = ?3, event_seq = ?4
         WHERE agent_id = ?1",
        params![agent, turn, event, event_seq],
    )?;
    if moved == 0 {
        tx.execute(
            "INSERT INTO block_watermarks
                 (agent_id, turn_id, event_id, event_seq, prev_turn_id, prev_event_id, prev_event_seq, seq)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5)",
            params![agent, turn, event, event_seq, seq::next(at)],
        )?;
    }
    Ok(())
}

/// A turn that ended `no_wakes` never reached the model, so it is no watermark: the turn before it
/// is back in the row.
fn restore_watermark(tx: &Transaction<'_>, agent: AgentId, turn: TurnId) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE block_watermarks
         SET turn_id = prev_turn_id, event_id = prev_event_id, event_seq = COALESCE(prev_event_seq, 0)
         WHERE agent_id = ?1 AND turn_id = ?2",
        params![agent, turn],
    )?;
    Ok(())
}

/// Marks a turn as having had the effective-action rule's one retry.
pub fn mark_retried(conn: &Connection, turn: TurnId) -> Result<(), StoreError> {
    conn.execute("UPDATE turns SET retried = 1 WHERE id = ?1", params![turn])?;
    Ok(())
}

/// One finished model call's tokens, added to its running turn's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallTokens {
    pub input: u64,
    /// Output including thinking.
    pub output: u64,
    pub thinking: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Adds one finished model call's tokens to a running turn, so its use shows before the turn
/// ends. The turn's commit then writes the engine's totals over these.
pub fn add_call_tokens(
    conn: &Connection,
    turn: TurnId,
    tokens: &CallTokens,
) -> Result<(), StoreError> {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    conn.execute(
        "UPDATE turns SET input_tokens = input_tokens + ?2, output_tokens = output_tokens + ?3,
             thinking_tokens = thinking_tokens + ?4, cache_read_tokens = cache_read_tokens + ?5,
             cache_write_tokens = cache_write_tokens + ?6
         WHERE id = ?1 AND status = 'running'",
        params![
            turn,
            n(tokens.input),
            n(tokens.output),
            n(tokens.thinking),
            n(tokens.cache_read),
            n(tokens.cache_write)
        ],
    )?;
    Ok(())
}

/// The first line of a close-out, cut to [`CLOSE_OUT_SUMMARY_CHARS`].
fn close_out_summary(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() <= CLOSE_OUT_SUMMARY_CHARS {
        line.to_owned()
    } else {
        let mut cut: String = line.chars().take(CLOSE_OUT_SUMMARY_CHARS - 1).collect();
        cut.push('…');
        cut
    }
}

/// Commits a finished turn in one transaction: the row, its events and its wakes.
pub fn finish_turn(conn: &mut Connection, f: &FinishedTurn) -> Result<(), StoreError> {
    if f.status == TurnStatus::Running {
        return Err(StoreError::Invalid {
            column: "turns.status",
            message: "a finished turn can't be running".to_owned(),
        });
    }
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE turns SET status = ?2, outcome_reason = ?3, effective_actions = ?4, retried = ?5,
             model = COALESCE(?6, model), model_calls = ?7, input_tokens = ?8, output_tokens = ?9,
             thinking_tokens = ?10, cache_read_tokens = ?11, cache_write_tokens = ?12,
             cost_usd = ?13, ended_at = ?14
         WHERE id = ?1 AND status = 'running'",
        params![
            f.turn,
            f.status,
            f.reason,
            json("turns.effective_actions", &f.actions)?,
            i64::from(f.retried),
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
    if f.reason.as_deref() == Some(NO_WAKES) {
        restore_watermark(&tx, f.agent, f.turn)?;
    }
    for text in f.close_outs.iter().filter(|t| !t.trim().is_empty()) {
        insert_event(
            &tx,
            kind::CLOSE_OUT,
            f.agent,
            None,
            Some(f.turn),
            &close_out_summary(text),
            &serde_json::json!({ "text": text }),
            f.at,
        )?;
    }
    if let Some(limit) = &f.limit {
        insert_event(
            &tx,
            kind::LIMIT_HIT,
            f.agent,
            None,
            Some(f.turn),
            &limit.reason,
            &serde_json::json!({ "limit": limit.limit, "counters": limit.counters }),
            f.at,
        )?;
    }
    let mut payload = serde_json::json!({ "status": f.status, "reason": f.reason });
    if let Some(detail) = &f.detail {
        payload["detail"] = serde_json::Value::String(detail.clone());
    }
    let summary = match &f.reason {
        Some(reason) => format!("turn {}: {reason}", f.status),
        None => format!("turn {}", f.status),
    };
    insert_event(
        &tx,
        kind::TURN_ENDED,
        f.agent,
        None,
        Some(f.turn),
        &summary,
        &payload,
        f.at,
    )?;
    if f.release_wakes {
        wakes::release_unconsumed(&tx, f.turn)?;
    } else {
        wakes::consume_wakes(&tx, f.turn, f.at)?;
    }
    tx.commit()?;
    Ok(())
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Records one of the seat's events outside a turn's commit.
pub fn record_event(conn: &Connection, e: &SeatEvent) -> Result<EventId, StoreError> {
    insert_event(
        conn, e.kind, e.agent, None, e.turn, &e.summary, &e.payload, e.at,
    )
}

/// Records `seat.cleared` for `agent` (you ran `/clear`), with the last turn before it. `after_turn`
/// is that turn's `seq`; before the sequence existed it was the turn's ID, which is the same number.
pub fn record_cleared(
    conn: &Connection,
    agent: AgentId,
    summary: &str,
    at: Timestamp,
) -> Result<EventId, StoreError> {
    let after_turn: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE agent_id = ?1",
        params![agent],
        |r| r.get(0),
    )?;
    insert_event(
        conn,
        kind::CLEARED,
        agent,
        None,
        None,
        summary,
        &serde_json::json!({ "after_turn": after_turn }),
        at,
    )
}

/// Startup: every `running` turn of `agent` becomes `interrupted` with reason
/// [`REASON_APP_STOPPED`], and its wakes go back in the inbox. One transaction.
pub fn interrupt_running(
    conn: &mut Connection,
    agent: AgentId,
    at: Timestamp,
) -> Result<Vec<Interrupted>, StoreError> {
    let tx = conn.transaction()?;
    let rows = interrupt_running_in(&tx, agent, at)?;
    tx.commit()?;
    Ok(rows)
}

/// [`interrupt_running`] inside the caller's transaction.
pub(crate) fn interrupt_running_in(
    tx: &Connection,
    agent: AgentId,
    at: Timestamp,
) -> Result<Vec<Interrupted>, StoreError> {
    let rows = {
        let mut stmt = tx.prepare(
            "UPDATE turns SET status = 'interrupted', outcome_reason = ?2, ended_at = ?3
             WHERE agent_id = ?1 AND status = 'running'
             RETURNING id, message_id",
        )?;
        let rows = stmt.query_map(params![agent, REASON_APP_STOPPED, to_millis(at)], |r| {
            Ok(Interrupted {
                turn: r.get(0)?,
                message: r.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for t in &rows {
        wakes::release_unconsumed(tx, t.turn)?;
        insert_event(
            tx,
            kind::TURN_ENDED,
            agent,
            None,
            Some(t.turn),
            "turn interrupted: app_stopped",
            &serde_json::json!({ "status": "interrupted", "reason": REASON_APP_STOPPED }),
            at,
        )?;
    }
    Ok(rows)
}

/// The turns of `agent` a stop or a quit cut short that startup hasn't handled yet, oldest first.
pub fn cut_short(conn: &Connection, agent: AgentId) -> Result<Vec<CutShort>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, message_id FROM turns
         WHERE agent_id = ?1 AND status = 'interrupted' AND outcome_reason IN (?2, ?3)
         ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![agent, REASON_APP_STOPPED, REASON_APP_QUIT], |r| {
        let message: Option<MessageId> = r.get(1)?;
        Ok(CutShort {
            turn: r.get(0)?,
            started_by: match message {
                Some(message) => StartedBy::User { message },
                None => StartedBy::AppEvent,
            },
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Marks a cut-short turn handled, so startup deals with it once.
pub fn mark_recovered(conn: &Connection, turn: TurnId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE turns SET outcome_reason = outcome_reason || ?2
         WHERE id = ?1 AND outcome_reason IN (?3, ?4)",
        params![turn, RECOVERED_SUFFIX, REASON_APP_STOPPED, REASON_APP_QUIT],
    )?;
    Ok(())
}

/// How many of `agent`'s wakes wait in the inbox, neither delivered nor consumed.
pub fn pending_wake_count(conn: &Connection, agent: AgentId) -> Result<u32, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM wakes
         WHERE target_agent_id = ?1 AND delivered_at IS NULL AND consumed_at IS NULL",
        params![agent],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The `seat.turn_started` event of `agent`'s last turn before `before`, with that turn: the
/// context block's "since your last turn" watermark.
pub fn last_turn_started_event(
    conn: &Connection,
    agent: AgentId,
    before: TurnId,
) -> Result<Option<(TurnId, EventId)>, StoreError> {
    let before = seq::of(conn, "turns", before.get(), i64::MIN)?;
    Ok(conn
        .query_row(
            "SELECT turn_id, id FROM events
             WHERE kind = ?1 AND agent_id = ?2 AND turn_id IN (SELECT id FROM turns WHERE seq < ?3)
             ORDER BY seq DESC
             LIMIT 1",
            params![kind::TURN_STARTED, agent, before],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}
