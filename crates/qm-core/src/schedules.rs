//! Scheduled wakes (`schedule` / `cancel_schedule`).
//!
//! - **Stored in UTC with your time zone.** A schedule's due time is kept as a UTC instant, with
//!   the IANA name of the time zone the app runs in, so it is shown in your time.
//! - **Armed on the scheduler.** Every pending schedule is registered with
//!   [`Scheduler::at_time`] under `schedule:SC-7`, at startup ([`ScheduleService::start`]) and when
//!   it is made. `at_time` re-checks the wall clock, so a Mac that slept fires it as it wakes, and
//!   a time already past fires at once.
//! - **Firing** posts a `schedule_due` wake to the schedule's owner (dedupe key `schedule:SC-7`)
//!   and marks the row fired, in one transaction; only a pending row fires, so a schedule fires
//!   at most once and a cancelled one never.
//! - **Late.** A schedule that fires more than [`LATE_AFTER`] after its due time (it came due while
//!   the app was closed, or the Mac was asleep) is marked late, and its wake says so, with the time
//!   it was due.

use crate::inbox::{Inbox, Notify};
use crate::scheduler::Scheduler;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::schedules::{self, NewSchedule, ScheduleRow};
use qm_store::wakes::NewWake;
use qm_tools::defs::Scheduled;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::clock::Clock;
use qm_types::domain::{ScheduleStatus, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::ScheduleId;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// A schedule that fires this long after its due time or later is marked late.
pub const LATE_AFTER: Duration = Duration::from_secs(60);

/// A schedule was made.
pub const SCHEDULE_ADDED: &str = "schedule.added";
/// A schedule was cancelled.
pub const SCHEDULE_CANCELLED: &str = "schedule.cancelled";
/// A schedule fired.
pub const SCHEDULE_FIRED: &str = "schedule.fired";

/// The time zone name stored when the system's has none.
const UTC: &str = "UTC";

/// What the schedule service runs on.
#[derive(Clone, Debug)]
pub struct ScheduleDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub inbox: Inbox,
    pub scheduler: Scheduler,
}

/// Makes, cancels, arms and fires schedules. Cheap to clone.
#[derive(Clone)]
pub struct ScheduleService {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    clock: Arc<dyn Clock>,
    inbox: Inbox,
    scheduler: Scheduler,
    tz: TimeZone,
}

impl fmt::Debug for ScheduleService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScheduleService").finish_non_exhaustive()
    }
}

/// The scheduler key for a schedule.
pub fn job_key(id: ScheduleId) -> String {
    format!("schedule:{id}")
}

impl ScheduleService {
    /// The service, in the system's time zone.
    pub fn new(deps: ScheduleDeps) -> Self {
        let tz = TimeZone::system();
        Self {
            inner: Arc::new(Inner {
                db: deps.db,
                clock: deps.clock,
                inbox: deps.inbox,
                scheduler: deps.scheduler,
                tz,
            }),
        }
    }

    /// At startup: arms every pending schedule. Those that came due while the app was closed
    /// fire at once, marked late. Returns how many were armed.
    pub async fn start(&self) -> Result<usize, StoreError> {
        let pending = self.inner.db.call(|conn| schedules::pending(conn)).await?;
        let n = pending.len();
        for row in pending {
            self.arm(row.id, row.due_at);
        }
        Ok(n)
    }

    /// `schedule`: stores a schedule for the caller and arms it. Refused when `due` isn't in the
    /// future.
    pub async fn add(
        &self,
        caller: &Caller,
        due: Timestamp,
        what: String,
    ) -> Result<Scheduled, ToolFailure> {
        let at = self.inner.clock.now();
        if due <= at {
            return Err(Refusal::new(format!(
                "the time must be in the future (it is now {at}, and {due} has passed)"
            ))
            .into());
        }
        let owner = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let tz = self.tz_name();
        let shown = local_time(due, &tz);
        let id = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let id = schedules::insert(
                    &tx,
                    &NewSchedule {
                        owner,
                        due_at: due,
                        tz: &tz,
                        what: &what,
                        turn,
                        at,
                    },
                )?;
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: SCHEDULE_ADDED,
                        agent: Some(owner),
                        task: None,
                        turn,
                        summary: &format!("Schedule {id} set for {shown}: {what}"),
                        payload: serde_json::json!({
                            "schedule": id, "due": due, "tz": tz, "what": what,
                        }),
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(id)
            })
            .await
            .map_err(|e| failed(&e))?;
        self.arm(id, due);
        Ok(Scheduled { schedule: id, due })
    }

    /// `cancel_schedule`: cancels one of the caller's pending schedules.
    pub async fn cancel(&self, caller: &Caller, id: ScheduleId) -> Result<(), ToolFailure> {
        let owner = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.inner.clock.now();
        let outcome = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let Some(row) = schedules::get(&tx, id)? else {
                    return Ok(Err(format!("{id} doesn't exist")));
                };
                if row.owner != owner {
                    return Ok(Err(format!("{id} isn't one of your schedules")));
                }
                match row.status {
                    ScheduleStatus::Fired => return Ok(Err(format!("{id} has already fired"))),
                    ScheduleStatus::Cancelled => {
                        return Ok(Err(format!("{id} was already cancelled")));
                    }
                    ScheduleStatus::Pending => {}
                }
                if !schedules::cancel(&tx, id)? {
                    return Ok(Err(format!("{id} is no longer pending")));
                }
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: SCHEDULE_CANCELLED,
                        agent: Some(owner),
                        task: None,
                        turn,
                        summary: &format!("Schedule {id} cancelled"),
                        payload: serde_json::json!({"schedule": id}),
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(Ok(()))
            })
            .await
            .map_err(|e| failed(&e))?;
        outcome.map_err(|reason| ToolFailure::from(Refusal::new(reason)))?;
        self.inner.scheduler.cancel(&job_key(id));
        Ok(())
    }

    /// Registers the schedule's job; a time already past fires at once.
    fn arm(&self, id: ScheduleId, due: Timestamp) {
        let service = self.clone();
        self.inner.scheduler.at_time(
            job_key(id),
            due,
            Box::new(move || {
                Box::pin(async move {
                    if let Err(e) = service.fire(id).await {
                        tracing::error!(schedule = %id, error = %e, "couldn't fire a schedule");
                    }
                })
            }),
        );
    }

    /// Fires a pending schedule: the owner's `schedule_due` wake and the row marked fired (late
    /// when past due), in one transaction, then the owner's loop is signalled. A schedule that
    /// isn't pending any more (cancelled, or already fired) does nothing.
    pub async fn fire(&self, id: ScheduleId) -> Result<bool, StoreError> {
        let at = self.inner.clock.now();
        let fired = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let Some(row) = schedules::get(&tx, id)? else {
                    return Ok(None);
                };
                if row.status != ScheduleStatus::Pending {
                    return Ok(None);
                }
                let late = is_late(row.due_at, at);
                let mut notify = Notify::new();
                Inbox::post(&tx, &due_wake(&row, late, at), &mut notify)?;
                schedules::mark_fired(&tx, id, at, late)?;
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: SCHEDULE_FIRED,
                        agent: Some(row.owner),
                        task: None,
                        turn: None,
                        summary: &format!(
                            "Schedule {id} fired{}: {}",
                            if late { " late" } else { "" },
                            row.what
                        ),
                        payload: serde_json::json!({
                            "schedule": id, "due": row.due_at, "late": late,
                        }),
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(Some(notify))
            })
            .await?;
        Ok(match fired {
            Some(notify) => {
                self.inner.inbox.notify(&notify);
                true
            }
            None => false,
        })
    }

    /// The IANA name of the service's time zone, or "UTC".
    fn tz_name(&self) -> String {
        self.inner
            .tz
            .iana_name()
            .map_or_else(|| UTC.to_owned(), str::to_owned)
    }
}

/// Whether a schedule due at `due` and firing at `at` fires late.
pub fn is_late(due: Timestamp, at: Timestamp) -> bool {
    let grace = jiff::SignedDuration::try_from(LATE_AFTER).unwrap_or(jiff::SignedDuration::ZERO);
    at.duration_since(due) >= grace
}

/// The `schedule_due` wake for the schedule's owner.
fn due_wake(row: &ScheduleRow, late: bool, at: Timestamp) -> NewWake {
    let due = local_time(row.due_at, &row.tz);
    let (text, short) = if late {
        (
            format!(
                "{} fired late: it was due at {due}, when the app wasn't running. It was for: {}",
                row.id, row.what
            ),
            format!("{} due (late, was {due})", row.id),
        )
    } else {
        (
            format!("{} is due ({due}). It was for: {}", row.id, row.what),
            format!("{} due", row.id),
        )
    };
    NewWake {
        target: row.owner,
        kind: WakeKind::ScheduleDue,
        payload: serde_json::json!({
            "text": text,
            "short": short,
            "schedule": row.id,
            "due": row.due_at,
            "tz": row.tz,
            "late": late,
        }),
        dedupe_key: job_key(row.id),
        task: None,
        at,
    }
}

/// `due` in the named time zone, as "Mon 19 Oct 14:00 Europe/London"; UTC when the name is
/// unknown.
pub fn local_time(due: Timestamp, tz: &str) -> String {
    let zone = TimeZone::get(tz).unwrap_or(TimeZone::UTC);
    format!("{} {tz}", due.to_zoned(zone).strftime("%a %-d %b %H:%M"))
}

fn failed(e: &StoreError) -> ToolFailure {
    ToolFailure::Failed(e.to_string())
}
