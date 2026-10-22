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

/// Whether `event` already has a channel row or an explicit-forgetting delivery marker.
/// The latter also protects batches read by the pump before their tasks were forgotten.
pub fn has_source_event(conn: &Connection, event: EventId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM parent_channel
                        WHERE json_extract(facts, '$.source_event') = ?1) \
         OR EXISTS (SELECT 1 FROM forgotten_parent_facts WHERE id = ?1)",
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
    pub seq: i64,
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
    /// The seq of the last event looked at, matching or not: the next scan starts after it.
    pub through: Option<i64>,
}

type RawFact = (
    EventId,
    i64,
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
        r.get(8)?,
    ))
}

fn parse_fact(raw: RawFact) -> Result<FactEvent, StoreError> {
    let (id, at, seq, kind, task, task_kind, correlation, secondmate, payload) = raw;
    Ok(FactEvent {
        id,
        seq,
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
    "SELECT e.id, e.at, e.seq, e.kind, t.id, t.kind, r.id, r.secondmate_id, e.payload
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

/// Events with a seq after `after` whose kind is one of `kinds`, on tasks that belong to a
/// request: at most `limit`, oldest first. Looks at events by seq, not by ID, so a cursor stays
/// valid after the event it names has been pruned.
pub fn events_after(
    conn: &Connection,
    after: i64,
    kinds: &[&str],
    limit: usize,
) -> Result<EventScan, StoreError> {
    let Some(newest) = conn.query_row("SELECT MAX(seq) FROM events", [], |r| {
        r.get::<_, Option<i64>>(0)
    })?
    else {
        return Ok(EventScan::default());
    };
    let mut values: Vec<rusqlite::types::Value> = vec![after.into(), newest.into()];
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
        rows.last().map(|e| e.seq)
    } else {
        Some(newest)
    };
    Ok(EventScan { rows, through })
}

/// The pump's read position: the seq of the newest event it has read, or `None` before its
/// first start.
pub fn cursor(conn: &Connection) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT event_seq FROM parent_channel_cursor WHERE seq = 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// Moves the pump's read position to `seq`. `UPDATE` then `INSERT`: an upsert on a mirrored
/// table fails (see AGENTS.md).
pub fn set_cursor(conn: &Connection, seq: i64) -> Result<(), StoreError> {
    let n = conn.execute(
        "UPDATE parent_channel_cursor SET event_seq = ?1 WHERE seq = 1",
        params![seq],
    )?;
    if n == 0 {
        conn.execute(
            "INSERT INTO parent_channel_cursor (seq, event_seq) VALUES (1, ?1)",
            params![seq],
        )?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{events_after, set_cursor};
    use crate::events::{self, NewEvent};
    use crate::mirror::Mirror;
    use crate::secondmates::{self, NewSecondmate};
    use crate::{connection, schema, seat, seq};
    use jiff::Timestamp;
    use qm_types::data_dir::DataDir;
    use rusqlite::{Connection, params};
    use std::path::{Path, PathBuf};

    /// A closed task of a routed request, with one `task.closed` event on it, in a database in a
    /// throwaway folder. Returns the folder, the connection and the event's seq.
    fn closed_routed_task(name: &str) -> (PathBuf, Connection, i64) {
        let dir =
            std::env::temp_dir().join(format!("qm-store-parent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("folder");
        let mut conn = connection::open(&dir.join("state.db")).expect("open");
        schema::migrate(&mut conn).expect("migrate");
        let now = Timestamp::now();
        let qm = seat::ensure_quartermaster(&conn, &dir, now)
            .expect("quartermaster")
            .id;
        let sm = secondmates::insert(
            &conn,
            &NewSecondmate {
                name: "Web",
                scope: "web",
                projects: &[],
                standing_branch: None,
                parent: qm,
                data_dir: &DataDir::new(&dir),
                at: now,
            },
        )
        .expect("secondmate");
        let turn = conn
            .query_row(
                "INSERT INTO turns (agent_id, started_by, started_at)
                 VALUES (?1, 'app_event', 0) RETURNING id",
                params![qm],
                |r| r.get::<_, i64>(0),
            )
            .expect("turn");
        let request = conn
            .query_row(
                "INSERT INTO secondmate_requests (secondmate_id, turn_id, request, user_words,
                     instructions, overrides, status, created_at, seq)
                 VALUES (?1, ?2, 'r', '', '[]', '{}', 'answered', 0, ?3) RETURNING id",
                params![sm.id, turn, seq::next_millis(0)],
                |r| r.get::<_, i64>(0),
            )
            .expect("request");
        conn.execute(
            "INSERT INTO projects (name, path, origin, default_branch, delivery_mode,
                 merge_method, created_at, seq)
             VALUES ('p', '/tmp/p', 'linked', 'main', 'direct_pr', 'merge', 0, ?1)",
            params![seq::next_millis(1)],
        )
        .expect("project");
        let project = conn.last_insert_rowid();
        let task = conn
            .query_row(
                "INSERT INTO tasks (project_id, supervisor_id, kind, title, goal, delivery_mode,
                     branch, status, created_at, seq, secondmate_request_id, closed_at, outcome)
                 VALUES (?1, ?2, 'build', 't', 'g', 'direct_pr', 'qm/task-1', 'closed', 0, ?3,
                     ?4, 0, 'landed')
                 RETURNING id",
                params![project, qm, seq::next_millis(2), request],
                |r| r.get::<_, i64>(0),
            )
            .expect("task");
        let event = events::insert(
            &conn,
            &NewEvent {
                kind: "task.closed",
                agent: Some(qm),
                task: Some(
                    qm_types::ids::TaskId::new(u32::try_from(task).expect("task number"))
                        .expect("task id"),
                ),
                turn: None,
                summary: "closed",
                payload: serde_json::json!({ "outcome": "landed" }),
                at: now,
            },
        )
        .expect("event");
        let seq_of_event = seq::of(&conn, "events", event.get(), 0).expect("seq");
        (dir, conn, seq_of_event)
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).expect("count")
    }

    fn restart(dir: &Path) -> Connection {
        Mirror::open(&dir.join("state.db")).expect("restart").0
    }

    #[test]
    fn a_closed_task_whose_fact_the_pump_has_not_read_survives_a_restart() {
        let (dir, _conn, _) = closed_routed_task("unread");
        let conn = restart(&dir);
        // The pump starts from the start: it must still find the fact and its routing.
        let scan = events_after(&conn, 0, &["task.closed"], 10).expect("scan");
        assert_eq!(scan.rows.len(), 1, "the fact is sent after the restart");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM secondmate_requests"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_completed_request_leaves_no_rows_on_disk_when_the_running_store_flushes() {
        let (dir, conn, fact) = closed_routed_task("flushed");
        let request = count(&conn, "SELECT COUNT(*) FROM secondmate_requests");
        assert_eq!(request, 1);
        let sm = conn
            .query_row("SELECT secondmate_id FROM secondmate_requests", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("secondmate");
        let request_id = conn
            .query_row("SELECT id FROM secondmate_requests", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("request");
        conn.execute(
            "INSERT INTO parent_channel (secondmate_id, correlation_id, facts, created_at, seq, id)
             VALUES (?1, ?2, '{}', 0, ?3, ?4)",
            params![
                sm,
                request_id,
                seq::next_millis(3),
                crate::ids::fresh(&conn, "parent_channel", qm_types::ids::space::ROW)
                    .expect("row id"),
            ],
        )
        .expect("row");
        drop(conn);
        let (mem, mut mirror) = Mirror::open(&dir.join("state.db")).expect("open");
        // The pump has read the fact: the task and its request are no longer kept.
        set_cursor(&mem, fact).expect("cursor");
        mirror.flush(&mem).expect("flush");
        let disk = connection::open(&dir.join("state.db")).expect("disk");
        assert_eq!(count(&disk, "SELECT COUNT(*) FROM parent_channel"), 0);
        assert_eq!(count(&disk, "SELECT COUNT(*) FROM secondmate_requests"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_closed_task_the_pump_has_read_is_dropped_at_the_next_restart() {
        let (dir, conn, fact) = closed_routed_task("read");
        set_cursor(&conn, fact).expect("cursor");
        drop(conn);
        let conn = restart(&dir);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM tasks"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM secondmate_requests"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
