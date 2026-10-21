//! Scheduled wakes (`schedule` / `cancel_schedule`).
//!
//! A schedule is stored in UTC (`due_at`, milliseconds) with the time zone of the person who set
//! it (`tz`, an IANA name), so it can be shown in their time. It is `pending` until it fires or is
//! cancelled; a schedule that fired after its due time (the app was closed, or the Mac asleep)
//! records `late`. Both transitions happen only from `pending`, so a schedule fires at most once
//! and a cancelled one never fires.

use crate::StoreError;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::ScheduleStatus;
use qm_types::ids::{AgentId, ScheduleId, TurnId};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// A schedule to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSchedule<'a> {
    /// The agent the wake goes to: the Quartermaster or a secondmate.
    pub owner: AgentId,
    pub due_at: Timestamp,
    /// The owner's time zone, an IANA name ("Europe/London"), or "UTC" when unknown.
    pub tz: &'a str,
    /// What the wake is for, in the owner's words.
    pub what: &'a str,
    /// The turn that called `schedule`.
    pub turn: Option<TurnId>,
    pub at: Timestamp,
}

/// A stored schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleRow {
    pub id: ScheduleId,
    pub owner: AgentId,
    pub due_at: Timestamp,
    pub tz: String,
    pub what: String,
    pub status: ScheduleStatus,
    pub turn: Option<TurnId>,
    pub fired_at: Option<Timestamp>,
    pub late: bool,
    pub created_at: Timestamp,
}

const COLUMNS: &str =
    "id, owner_agent_id, due_at, tz, what, status, created_turn_id, fired_at, late, created_at";

type RawRow = (
    ScheduleId,
    AgentId,
    i64,
    String,
    String,
    ScheduleStatus,
    Option<TurnId>,
    Option<i64>,
    bool,
    i64,
);

fn raw(r: &Row<'_>) -> rusqlite::Result<RawRow> {
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

fn row(raw: RawRow) -> Result<ScheduleRow, StoreError> {
    let (id, owner, due, tz, what, status, turn, fired, late, created) = raw;
    Ok(ScheduleRow {
        id,
        owner,
        due_at: from_millis("schedules.due_at", due)?,
        tz,
        what,
        status,
        turn,
        fired_at: fired
            .map(|ms| from_millis("schedules.fired_at", ms))
            .transpose()?,
        late,
        created_at: from_millis("schedules.created_at", created)?,
    })
}

/// Writes a pending schedule and returns its ID.
pub fn insert(conn: &Connection, new: &NewSchedule<'_>) -> Result<ScheduleId, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO schedules (owner_agent_id, due_at, tz, what, status, created_turn_id, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
        params![
            new.owner,
            to_millis(new.due_at),
            new.tz,
            new.what,
            ScheduleStatus::Pending,
            new.turn,
            to_millis(new.at),
            seq::next(new.at),
        ],
        |r| r.get(0),
    )?)
}

/// One schedule, in any state.
pub fn get(conn: &Connection, id: ScheduleId) -> Result<Option<ScheduleRow>, StoreError> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM schedules WHERE id = ?1"),
        params![id],
        raw,
    )
    .optional()?
    .map(row)
    .transpose()
}

/// Every pending schedule, soonest first.
pub fn pending(conn: &Connection) -> Result<Vec<ScheduleRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM schedules WHERE status = ?1 ORDER BY due_at, seq"
    ))?;
    let raws = stmt
        .query_map(params![ScheduleStatus::Pending], raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(row).collect()
}

/// Cancels a pending schedule. Returns `false` (and changes nothing) when it isn't pending.
pub fn cancel(conn: &Connection, id: ScheduleId) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE schedules SET status = ?2 WHERE id = ?1 AND status = ?3",
        params![id, ScheduleStatus::Cancelled, ScheduleStatus::Pending],
    )?;
    Ok(changed == 1)
}

/// Marks a pending schedule fired at `at`, `late` when that was after its due time. Returns
/// `false` (and changes nothing) when it isn't pending: it already fired or was cancelled.
pub fn mark_fired(
    conn: &Connection,
    id: ScheduleId,
    at: Timestamp,
    late: bool,
) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE schedules SET status = ?2, fired_at = ?3, late = ?4 WHERE id = ?1 AND status = ?5",
        params![
            id,
            ScheduleStatus::Fired,
            to_millis(at),
            late,
            ScheduleStatus::Pending
        ],
    )?;
    Ok(changed == 1)
}
