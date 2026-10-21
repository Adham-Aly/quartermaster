//! The parent channel and the request status it owns.
//!
//! Each update from a secondmate to the Quartermaster is one appended `parent_channel` row: `facts`
//! (JSON, always written by the app) and `judgment` (JSON or `NULL`, only from `parent_report`).
//! Rows are never updated. A request's correlation ID **is** its `secondmate_requests` row ID; this
//! module reads requests through its own queries and owns their status (`open → answered →
//! open`).
//!
//! Every function runs inside the caller's transaction, so a row, the status change it causes, its
//! event and its wake commit together.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{RequestStatus, TaskKind, TaskStatus};
use qm_types::ids::space;
use qm_types::ids::{AgentId, CorrelationId, EventId, SecondmateId, TaskId};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

/// A parent-channel row's ID.
pub type ParentRowId = i64;

/// A row to append.
#[derive(Clone, Debug, PartialEq)]
pub struct NewChannelRow<'a> {
    pub secondmate: SecondmateId,
    pub correlation: CorrelationId,
    /// The app's facts snapshot.
    pub facts: &'a serde_json::Value,
    /// The secondmate's judgment (`parent_report` only).
    pub judgment: Option<&'a serde_json::Value>,
    pub at: Timestamp,
}

/// A stored row.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelRow {
    pub id: ParentRowId,
    pub secondmate: SecondmateId,
    pub correlation: CorrelationId,
    pub facts: serde_json::Value,
    pub judgment: Option<serde_json::Value>,
    pub created_at: Timestamp,
}

/// Appends one row and returns its ID.
pub fn insert(conn: &Connection, row: &NewChannelRow<'_>) -> Result<ParentRowId, StoreError> {
    let facts = serde_json::to_string(row.facts).map_err(|source| StoreError::Json {
        column: "parent_channel.facts",
        source,
    })?;
    let judgment = row
        .judgment
        .map(serde_json::to_string)
        .transpose()
        .map_err(|source| StoreError::Json {
            column: "parent_channel.judgment",
            source,
        })?;
    Ok(conn.query_row(
        "INSERT INTO parent_channel (secondmate_id, correlation_id, facts, judgment, created_at, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
        params![
            row.secondmate,
            row.correlation,
            facts,
            judgment,
            to_millis(row.at),
            seq::next(row.at),
            ids::fresh(conn, "parent_channel", space::ROW)?,
        ],
        |r| r.get(0),
    )?)
}

type RawRow = (
    ParentRowId,
    SecondmateId,
    CorrelationId,
    String,
    Option<String>,
    i64,
);

fn parse_row(raw: RawRow) -> Result<ChannelRow, StoreError> {
    let (id, secondmate, correlation, facts, judgment, at) = raw;
    Ok(ChannelRow {
        id,
        secondmate,
        correlation,
        facts: serde_json::from_str(&facts).map_err(|source| StoreError::Json {
            column: "parent_channel.facts",
            source,
        })?,
        judgment: judgment
            .map(|j| serde_json::from_str(&j))
            .transpose()
            .map_err(|source| StoreError::Json {
                column: "parent_channel.judgment",
                source,
            })?,
        created_at: from_millis("parent_channel.created_at", at)?,
    })
}

const ROW_COLUMNS: &str = "id, secondmate_id, correlation_id, facts, judgment, created_at";

fn raw_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

/// The newest row for a correlation.
pub fn latest(conn: &Connection, corr: CorrelationId) -> Result<Option<ChannelRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {ROW_COLUMNS} FROM parent_channel WHERE correlation_id = ?1
                 ORDER BY seq DESC LIMIT 1"
            ),
            params![corr],
            raw_row,
        )
        .optional()?;
    raw.map(parse_row).transpose()
}

/// A secondmate's newest rows, newest first, at most `limit`.
pub fn recent_of(
    conn: &Connection,
    secondmate: SecondmateId,
    limit: usize,
) -> Result<Vec<ChannelRow>, StoreError> {
    let raws = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM parent_channel WHERE secondmate_id = ?1
             ORDER BY seq DESC LIMIT ?2"
        ))?;
        stmt.query_map(
            params![secondmate, i64::try_from(limit).unwrap_or(i64::MAX)],
            raw_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    raws.into_iter().map(parse_row).collect()
}

/// The pump's watermark: the newest event any row was written for.
pub fn watermark(conn: &Connection) -> Result<Option<EventId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT json_extract(p.facts, '$.source_event') FROM parent_channel AS p
             JOIN events AS e ON e.id = json_extract(p.facts, '$.source_event')
             ORDER BY e.seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether some row was already written for `event` (the pump's replay guard).
pub fn has_source_event(conn: &Connection, event: EventId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM parent_channel
                        WHERE json_extract(facts, '$.source_event') = ?1)",
        params![event],
        |r| r.get(0),
    )?)
}

/// The oldest event of `kind`, if any (where the pump starts before it has written any row: the
/// first routed request, since no task can belong to a request before it).
pub fn first_event_of(conn: &Connection, kind: &str) -> Result<Option<EventId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM events WHERE kind = ?1 ORDER BY seq LIMIT 1",
            params![kind],
            |r| r.get(0),
        )
        .optional()?)
}

/// The newest event's ID, if any (where a first run starts, so history isn't replayed).
pub fn newest_event(conn: &Connection) -> Result<Option<EventId>, StoreError> {
    Ok(conn
        .query_row("SELECT id FROM events ORDER BY seq DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// A fact event on a task that belongs to a routed request.
#[derive(Clone, Debug, PartialEq)]
pub struct FactEvent {
    pub id: EventId,
    pub at: Timestamp,
    pub kind: String,
    pub task: TaskId,
    pub task_kind: TaskKind,
    pub correlation: CorrelationId,
    pub secondmate: SecondmateId,
    pub payload: serde_json::Value,
}

/// What [`events_after`] read, and how far it looked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventScan {
    /// Matching events, oldest first.
    pub rows: Vec<FactEvent>,
    /// Every event up to here was looked at, matching or not: the next scan starts after it.
    pub through: Option<EventId>,
}

type RawFact = (
    EventId,
    i64,
    String,
    TaskId,
    TaskKind,
    CorrelationId,
    SecondmateId,
    String,
);

fn raw_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawFact> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

fn parse_fact(raw: RawFact) -> Result<FactEvent, StoreError> {
    let (id, at, kind, task, task_kind, correlation, secondmate, payload) = raw;
    Ok(FactEvent {
        id,
        at: from_millis("events.at", at)?,
        kind,
        task,
        task_kind,
        correlation,
        secondmate,
        payload: serde_json::from_str(&payload).map_err(|source| StoreError::Json {
            column: "events.payload",
            source,
        })?,
    })
}

const FACT_SELECT: &str =
    "SELECT e.id, e.at, e.kind, t.id, t.kind, r.id, r.secondmate_id, e.payload
     FROM events e
     JOIN tasks t ON t.id = e.task_id
     JOIN secondmate_requests r ON r.id = t.secondmate_request_id";

fn kinds_in(kinds: &[&str], first_param: usize) -> String {
    if kinds.is_empty() {
        return "0".to_owned();
    }
    let list: Vec<String> = (0..kinds.len())
        .map(|i| format!("?{}", first_param + i))
        .collect();
    format!("e.kind IN ({})", list.join(", "))
}

/// Events after `after` whose kind is one of `kinds`, on tasks that belong to a request: at most
/// `limit`, oldest first.
pub fn events_after(
    conn: &Connection,
    after: Option<EventId>,
    kinds: &[&str],
    limit: usize,
) -> Result<EventScan, StoreError> {
    let Some(newest) = newest_event(conn)? else {
        return Ok(EventScan::default());
    };
    let after = after.map_or(Ok(0), |id| seq::of(conn, "events", id.get(), 0))?;
    let newest_seq = seq::of(conn, "events", newest.get(), i64::MAX)?;
    let mut values: Vec<rusqlite::types::Value> = vec![after.into(), newest_seq.into()];
    values.extend(
        kinds
            .iter()
            .map(|k| rusqlite::types::Value::from((*k).to_owned())),
    );
    let raws = {
        let mut stmt = conn.prepare(&format!(
            "{FACT_SELECT}
             WHERE e.seq > ?1 AND e.seq <= ?2 AND {}
             ORDER BY e.seq LIMIT {}",
            kinds_in(kinds, 3),
            i64::try_from(limit).unwrap_or(i64::MAX)
        ))?;
        stmt.query_map(params_from_iter(values.iter()), raw_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let rows = raws
        .into_iter()
        .map(parse_fact)
        .collect::<Result<Vec<_>, _>>()?;
    let through = if rows.len() >= limit {
        rows.last().map(|e| e.id)
    } else {
        Some(newest)
    };
    Ok(EventScan { rows, through })
}

/// A request, with who it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRecord {
    pub id: CorrelationId,
    pub secondmate: SecondmateId,
    /// The secondmate's agent.
    pub agent: AgentId,
    /// The secondmate's supervisor: the Quartermaster's agent.
    pub parent: AgentId,
    pub secondmate_name: String,
    pub scope: String,
    pub request: String,
    pub status: RequestStatus,
}

/// A request by its correlation ID, joined to its secondmate and the secondmate's agent.
pub fn request(
    conn: &Connection,
    corr: CorrelationId,
) -> Result<Option<RequestRecord>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT r.id, r.secondmate_id, a.id, a.supervisor_id, s.name, s.scope, r.request,
                    r.status
             FROM secondmate_requests r
             JOIN secondmates s ON s.id = r.secondmate_id
             JOIN agents a ON a.id = (SELECT id FROM agents
                                      WHERE secondmate_id = s.id AND role = 'secondmate'
                                      ORDER BY seq DESC LIMIT 1)
             WHERE r.id = ?1",
            params![corr],
            |r| {
                Ok(RequestRecord {
                    id: r.get(0)?,
                    secondmate: r.get(1)?,
                    agent: r.get(2)?,
                    parent: r.get(3)?,
                    secondmate_name: r.get(4)?,
                    scope: r.get(5)?,
                    request: r.get(6)?,
                    status: r.get(7)?,
                })
            },
        )
        .optional()?)
}

/// Moves a request's status from `from` to `to` (a compare-and-swap); `false` when it wasn't
/// `from`.
pub fn set_request_status(
    conn: &Connection,
    corr: CorrelationId,
    from: RequestStatus,
    to: RequestStatus,
) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE secondmate_requests SET status = ?3 WHERE id = ?1 AND status = ?2",
        params![corr, from, to],
    )?;
    Ok(changed == 1)
}

/// One task of a request, as the snapshot shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotTask {
    pub id: TaskId,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub pr: Option<i64>,
    /// `tasks.checks_state`'s verdict (`passed`, `failed`, …), when delivery recorded one.
    pub checks: Option<String>,
}

/// Everything the core's facts snapshot (`qm_core::parent_channel::facts`) needs for one
/// correlation.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotInputs {
    pub correlation: CorrelationId,
    /// Every task of the request, oldest first.
    pub tasks: Vec<SnapshotTask>,
    /// The fact events on those tasks, oldest first (the newest [`SNAPSHOT_EVENTS`]).
    pub events: Vec<FactEvent>,
    /// Whether the secondmate's agent is in a turn now.
    pub secondmate_running: bool,
    /// The event that caused the row (`None` for a judgment row).
    pub source_event: Option<EventId>,
}

/// How many fact events a snapshot reads (its outcomes keep the newest 20 of what they map to).
pub const SNAPSHOT_EVENTS: usize = 200;

/// The snapshot's inputs for `corr`, as of `upto` (inclusive) when given.
pub fn snapshot_inputs(
    conn: &Connection,
    corr: CorrelationId,
    kinds: &[&str],
    upto: Option<EventId>,
) -> Result<SnapshotInputs, StoreError> {
    let tasks = {
        let mut stmt = conn.prepare(
            "SELECT id, kind, status, pr_number, json_extract(checks_state, '$.verdict')
             FROM tasks WHERE secondmate_request_id = ?1 ORDER BY seq",
        )?;
        stmt.query_map(params![corr], |r| {
            Ok(SnapshotTask {
                id: r.get(0)?,
                kind: r.get(1)?,
                status: r.get(2)?,
                pr: r.get(3)?,
                checks: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let upto_seq = upto.map_or(Ok(i64::MAX), |id| {
        seq::of(conn, "events", id.get(), i64::MAX)
    })?;
    let mut values: Vec<rusqlite::types::Value> = vec![corr.get().into(), upto_seq.into()];
    values.extend(
        kinds
            .iter()
            .map(|k| rusqlite::types::Value::from((*k).to_owned())),
    );
    let raws = {
        let mut stmt = conn.prepare(&format!(
            "{FACT_SELECT}
             WHERE r.id = ?1 AND e.seq <= ?2 AND {}
             ORDER BY e.seq DESC LIMIT {SNAPSHOT_EVENTS}",
            kinds_in(kinds, 3)
        ))?;
        stmt.query_map(params_from_iter(values.iter()), raw_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut events = raws
        .into_iter()
        .map(parse_fact)
        .collect::<Result<Vec<_>, _>>()?;
    events.reverse();
    let secondmate_running: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM agents a
                            JOIN secondmate_requests r ON r.secondmate_id = a.secondmate_id
                            WHERE r.id = ?1 AND a.role = 'secondmate' AND a.status = 'running')",
            params![corr],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    Ok(SnapshotInputs {
        correlation: corr,
        tasks,
        events,
        secondmate_running,
        source_event: upto,
    })
}
