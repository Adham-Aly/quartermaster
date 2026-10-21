//! The event log: append-only, the audit trail and the source of the context block's "since your
//! last turn" sections.
//!
//! **The convention to follow** when recording something the Quartermaster should see:
//! `events.summary` is the line exactly as the context block shows it, in the app's voice ("you
//! approved the merge of T-0038 at 77d1e0a"), and `events.kind` starts with the section's prefix
//! (`you.`, `task.`, `pr.`, `checks.`, `secondmate.`, `model.`, `outcome.`). `seat.*` events are
//! the seat's own bookkeeping and never appear in the block.
//!
//! The notice under one of your messages is an event too (`seat.notice`), and so is your answer on
//! it (`you.answered_notice`): [`notices`] folds them into each message's notice state.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::ids::space;
use qm_types::ids::{AgentId, EventId, MessageId, TaskId, TurnId};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::collections::BTreeMap;

/// `seat.notice`: a notice under one of your messages (the seam's `event_kind::NOTICE`).
pub const NOTICE: &str = "seat.notice";
/// `you.answered_notice`: your answer on a notice (the seam's `event_kind::NOTICE_ANSWERED`).
pub const NOTICE_ANSWERED: &str = "you.answered_notice";
/// `seat.turn_started` (the seam's `event_kind::TURN_STARTED`).
pub const TURN_STARTED: &str = "seat.turn_started";

/// An event to record.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent<'a> {
    pub kind: &'a str,
    pub agent: Option<AgentId>,
    pub task: Option<TaskId>,
    pub turn: Option<TurnId>,
    /// The line as the context block shows it, for kinds with a section prefix.
    pub summary: &'a str,
    pub payload: serde_json::Value,
    pub at: Timestamp,
}

/// A recorded event.
#[derive(Clone, Debug, PartialEq)]
pub struct EventRow {
    pub id: EventId,
    pub at: Timestamp,
    pub kind: String,
    pub agent: Option<AgentId>,
    pub task: Option<TaskId>,
    pub turn: Option<TurnId>,
    pub summary: String,
    pub payload: serde_json::Value,
}

/// Some of the matching events, newest first, and how many more matched.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventPage {
    pub rows: Vec<EventRow>,
    /// Matching events left out by the limit.
    pub more: usize,
}

/// Appends one event. Call it inside the transaction that records the fact.
pub fn insert(conn: &Connection, event: &NewEvent<'_>) -> Result<EventId, StoreError> {
    let payload = serde_json::to_string(&event.payload).map_err(|source| StoreError::Json {
        column: "events.payload",
        source,
    })?;
    Ok(conn.query_row(
        "INSERT INTO events (at, kind, agent_id, task_id, turn_id, summary, payload, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         RETURNING id",
        params![
            to_millis(event.at),
            event.kind,
            event.agent,
            event.task,
            event.turn,
            event.summary,
            payload,
            seq::next(event.at),
            ids::fresh(conn, "events", space::ROW)?,
        ],
        |r| r.get(0),
    )?)
}

const COLUMNS: &str = "id, at, kind, agent_id, task_id, turn_id, summary, payload";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
    Ok(RawEvent {
        id: r.get(0)?,
        at: r.get(1)?,
        kind: r.get(2)?,
        agent: r.get(3)?,
        task: r.get(4)?,
        turn: r.get(5)?,
        summary: r.get(6)?,
        payload: r.get(7)?,
    })
}

struct RawEvent {
    id: EventId,
    at: i64,
    kind: String,
    agent: Option<AgentId>,
    task: Option<TaskId>,
    turn: Option<TurnId>,
    summary: String,
    payload: String,
}

impl RawEvent {
    fn parse(self) -> Result<EventRow, StoreError> {
        Ok(EventRow {
            id: self.id,
            at: from_millis("events.at", self.at)?,
            kind: self.kind,
            agent: self.agent,
            task: self.task,
            turn: self.turn,
            summary: self.summary,
            payload: serde_json::from_str(&self.payload).map_err(|source| StoreError::Json {
                column: "events.payload",
                source,
            })?,
        })
    }
}

/// `kind LIKE 'prefix%'` for each prefix, OR-ed, with `_` and `%` in a prefix taken literally.
fn prefix_filter(prefixes: &[&str], first_param: usize) -> (String, Vec<String>) {
    if prefixes.is_empty() {
        return ("0".to_owned(), Vec::new());
    }
    let clauses: Vec<String> = (0..prefixes.len())
        .map(|i| format!("kind LIKE ?{} ESCAPE '\\'", first_param + i))
        .collect();
    let patterns = prefixes
        .iter()
        .map(|p| {
            let escaped = p
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("{escaped}%")
        })
        .collect();
    (format!("({})", clauses.join(" OR ")), patterns)
}

/// Events after creation order `after` (and before `before`, when given; both are `seq`s, so they
/// hold across a restart) whose kind starts with one of `prefixes`: at most `limit`, newest first,
/// with the count of the rest.
pub fn since(
    conn: &Connection,
    after: i64,
    before: Option<i64>,
    prefixes: &[&str],
    limit: usize,
) -> Result<EventPage, StoreError> {
    let (filter, patterns) = prefix_filter(prefixes, 3);
    let mut values: Vec<rusqlite::types::Value> =
        vec![after.into(), before.unwrap_or(i64::MAX).into()];
    values.extend(patterns.into_iter().map(rusqlite::types::Value::from));

    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM events WHERE seq > ?1 AND seq < ?2 AND {filter}"),
        params_from_iter(values.iter()),
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM events WHERE seq > ?1 AND seq < ?2 AND {filter}
         ORDER BY seq DESC LIMIT {}",
        i64::try_from(limit).unwrap_or(i64::MAX)
    ))?;
    let rows = stmt
        .query_map(params_from_iter(values.iter()), row)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(RawEvent::parse)
        .collect::<Result<Vec<_>, _>>()?;
    let total = usize::try_from(total).unwrap_or(0);
    Ok(EventPage {
        more: total.saturating_sub(rows.len()),
        rows,
    })
}

/// The `limit` newest events whose kind starts with `prefix`, newest first.
pub fn latest(conn: &Connection, prefix: &str, limit: usize) -> Result<Vec<EventRow>, StoreError> {
    Ok(since(conn, 0, None, &[prefix], limit)?.rows)
}

/// The `limit` newest events whose kind starts with `prefix` and whose `payload.supervisor` is
/// `supervisor`, newest first: an orchestrator's own outcomes (plan 5, step E).
pub fn latest_of(
    conn: &Connection,
    prefix: &str,
    supervisor: AgentId,
    limit: usize,
) -> Result<Vec<EventRow>, StoreError> {
    let (filter, patterns) = prefix_filter(&[prefix], 2);
    let mut values: Vec<rusqlite::types::Value> = vec![supervisor.get().into()];
    values.extend(patterns.into_iter().map(rusqlite::types::Value::from));
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM events
         WHERE json_extract(payload, '$.supervisor') = ?1 AND {filter}
         ORDER BY seq DESC LIMIT {}",
        i64::try_from(limit).unwrap_or(i64::MAX)
    ))?;
    stmt.query_map(params_from_iter(values.iter()), row)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(RawEvent::parse)
        .collect::<Result<Vec<_>, _>>()
}

/// The creation order (`seq`) of the `seat.turn_started` event of `turn`.
pub fn turn_started_seq(conn: &Connection, turn: TurnId) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT seq FROM events WHERE kind = ?1 AND turn_id = ?2 ORDER BY seq LIMIT 1",
            params![TURN_STARTED, turn],
            |r| r.get(0),
        )
        .optional()?)
}

/// Why a notice was shown (`seat.notice`'s `cause`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeCause {
    /// No action after the retry.
    NoAction,
    /// The turn failed: a limit, a provider error, the session gone, the app quitting.
    Failed,
}

impl NoticeCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoAction => "no_action",
            Self::Failed => "failed",
        }
    }
}

/// Your answer on a notice (`you.answered_notice`'s `answer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeChoice {
    AskAgain,
    LeaveIt,
}

impl NoticeChoice {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AskAgain => "ask_again",
            Self::LeaveIt => "leave_it",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "ask_again" => Some(Self::AskAgain),
            "leave_it" => Some(Self::LeaveIt),
            _ => None,
        }
    }
}

/// The notice under one message: its newest `seat.notice`, and your answer on that notice if you
/// gave one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeState {
    pub event: EventId,
    pub cause: NoticeCause,
    /// One plain sentence, for a failed turn.
    pub reason: Option<String>,
    pub answer: Option<NoticeChoice>,
}

/// The notice state of each message in `messages` that has one.
pub fn notices(
    conn: &Connection,
    messages: &[MessageId],
) -> Result<BTreeMap<MessageId, NoticeState>, StoreError> {
    let mut out: BTreeMap<MessageId, NoticeState> = BTreeMap::new();
    if messages.is_empty() {
        return Ok(out);
    }
    // A notice or answer names its message in the payload; read both kinds for these messages,
    // oldest first, and fold: a newer notice replaces an older one (Ask again led to another
    // turn that again did nothing), and an answer applies to the notice before it.
    let placeholders = (0..messages.len())
        .map(|i| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values: Vec<rusqlite::types::Value> =
        vec![NOTICE.to_owned().into(), NOTICE_ANSWERED.to_owned().into()];
    values.extend(
        messages
            .iter()
            .map(|m| rusqlite::types::Value::from(m.get())),
    );
    let mut stmt = conn.prepare(&format!(
        "SELECT id, kind, json_extract(payload, '$.message'), json_extract(payload, '$.cause'),
                json_extract(payload, '$.reason'), json_extract(payload, '$.answer')
         FROM events
         WHERE kind IN (?1, ?2) AND json_extract(payload, '$.message') IN ({placeholders})
         ORDER BY seq"
    ))?;
    let rows = stmt
        .query_map(params_from_iter(values.iter()), |r| {
            Ok((
                r.get::<_, EventId>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, MessageId>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, kind, message, cause, reason, answer) in rows {
        if kind == NOTICE {
            let cause = match cause.as_deref() {
                Some("failed") => NoticeCause::Failed,
                _ => NoticeCause::NoAction,
            };
            out.insert(
                message,
                NoticeState {
                    event: id,
                    cause,
                    reason,
                    answer: None,
                },
            );
        } else if let Some(state) = out.get_mut(&message)
            && state.answer.is_none()
        {
            state.answer = answer.as_deref().and_then(NoticeChoice::parse);
        }
    }
    Ok(out)
}
