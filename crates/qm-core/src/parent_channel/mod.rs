//! The parent channel: how a secondmate's work reaches the Quartermaster.
//!
//! Each update is one appended `parent_channel` row for one correlation: `facts`, **always written
//! by the app** (a snapshot of the request's tasks and outcomes, [`facts::snapshot`]), and
//! `judgment`, **only from the secondmate's `parent_report`** (`NULL` on fact rows). Every row
//! wakes the Quartermaster once (kind `parent_report`, dedupe `parent:C-0193:<row id>`) and writes
//! a `secondmate.facts` or `secondmate.report` event whose summary its context block shows.
//!
//! - **Facts** come from the pump ([`pump`]), which follows the append-only `events` table and
//!   turns each fact on a request's task (started, reported, held, stuck, landed, closed,
//!   failed, PR ready, checks passed or failed, merged) into a row, in its own transaction. Merge
//!   facts flow up the same way; settling also nudges the pump through [`ParentPort`] so a
//!   landing goes up at once.
//! - **Judgment** comes from [`ParentChannel::report`], behind `ToolHost::parent_report`.
//!
//! The Quartermaster sees a secondmate's crew but acts only through the secondmate: the task tools
//! refuse it with `check_direct_report`; this channel is how it hears about them.

pub mod facts;
pub mod pump;
pub mod wake;

use crate::correlation::{self, RequestFacts, StatusCause};
use crate::delivery::ports::ParentPort;
use crate::inbox::{Inbox, Notify};
use crate::settling::facts::Fact;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_policy::markers::neutralise;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::parent_channel::{self as store, FactEvent, NewChannelRow, ParentRowId};
use qm_store::wakes::NewWake;
use qm_tools::defs::Ack;
use qm_tools::handler::{Caller, ParentJudgment, ToolFailure};
use qm_types::clock::Clock;
use qm_types::domain::{Role, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::{CorrelationId, TaskId};
use rusqlite::Connection;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub use pump::PUMP_TICK;

/// `events.kind` of a fact row.
pub const FACTS_EVENT: &str = "secondmate.facts";
/// `events.kind` of a judgment row.
pub const REPORT_EVENT: &str = "secondmate.report";
/// How much of a summary the `secondmate.report` event line shows.
pub const EVENT_SUMMARY_CHARS: usize = 120;
/// How long a pump batch may be (events read per pass).
pub const PUMP_BATCH: usize = 100;
/// The pause before retrying after the store failed.
pub const RETRY_PAUSE: Duration = Duration::from_secs(5);

/// The parent channel. Cheap to clone.
#[derive(Clone)]
pub struct ParentChannel {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    clock: Arc<dyn Clock>,
    inbox: Inbox,
    nudge: tokio::sync::Notify,
    tz: TimeZone,
}

impl fmt::Debug for ParentChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParentChannel").finish_non_exhaustive()
    }
}

impl ParentChannel {
    pub fn new(db: Db, clock: Arc<dyn Clock>, inbox: Inbox) -> Self {
        let tz = TimeZone::system();
        Self {
            inner: Arc::new(Inner {
                db,
                clock,
                inbox,
                nudge: tokio::sync::Notify::new(),
                tz,
            }),
        }
    }

    /// "A fact was just written": the pump reads at once instead of within [`PUMP_TICK`]. Safe to
    /// call inside the fact's transaction: the pump's read queues behind it on the database
    /// thread.
    pub fn nudge(&self) {
        self.inner.nudge.notify_one();
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.db
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.inner.clock
    }

    pub(crate) fn nudged(&self) -> &tokio::sync::Notify {
        &self.inner.nudge
    }

    /// `parent_report`: the caller must be a secondmate reporting on one of its own requests. One
    /// transaction: a row with the **app's** facts and the judgment (neutralised), the status rule,
    /// the `secondmate.report` event and the Quartermaster's wake.
    pub async fn report(
        &self,
        caller: &Caller,
        judgment: ParentJudgment,
    ) -> Result<Ack, ToolFailure> {
        let Some(secondmate) = caller
            .secondmate
            .filter(|_| caller.role == Role::Secondmate)
        else {
            return Err(Refusal::new("only a secondmate reports on the parent channel").into());
        };
        let corr = judgment.correlation;
        let at = self.inner.clock.now();
        let tz = self.inner.tz.clone();
        let (outcome, notify) = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let req = match correlation::check_report_correlation(&tx, secondmate, corr) {
                    Ok(req) => req,
                    Err(refused) => return Ok((Err(refused), Notify::new())),
                };
                let mut notify = Notify::new();
                record_judgment(&tx, &mut notify, &req, &judgment, &tz, at)?;
                tx.commit()?;
                Ok((Ok(()), notify))
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        outcome?;
        self.inner.inbox.notify(&notify);
        Ok(Ack {
            note: format!("Reported on {corr}; the Quartermaster sees it with your tasks' facts."),
        })
    }

    /// One fact event as a row, in its own transaction; `true` when a row was written. Skips an
    /// event some row already came from (a replay after a crash).
    pub(crate) async fn record_event(&self, event: FactEvent) -> Result<bool, StoreError> {
        let Some(outcome) = facts::outcome_of(&event) else {
            return Ok(false);
        };
        let at = self.inner.clock.now();
        let tz = self.inner.tz.clone();
        let (written, notify) = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let mut notify = Notify::new();
                let written = record_fact(&tx, &mut notify, &event, &outcome, &tz, at)?;
                tx.commit()?;
                Ok((written, notify))
            })
            .await?;
        self.inner.inbox.notify(&notify);
        Ok(written)
    }
}

/// Settling's hook: a settled task's fact is already an event (`pr.merged`, `task.status` →
/// `landed`) in this transaction, so the pump sends it up; the hook only wakes the pump so it goes
/// at once.
impl ParentPort for ParentChannel {
    fn task_settled(&self, conn: &Connection, task: TaskId, fact: &Fact) -> Result<(), StoreError> {
        let _ = (conn, task, fact);
        self.nudge();
        Ok(())
    }
}

fn json(
    column: &'static str,
    value: &impl serde::Serialize,
) -> Result<serde_json::Value, StoreError> {
    serde_json::to_value(value).map_err(|source| StoreError::Json { column, source })
}

/// The Quartermaster's wake for a row.
fn channel_wake(
    req: &RequestFacts,
    row: ParentRowId,
    task: Option<TaskId>,
    text: &str,
    short: &str,
    at: Timestamp,
) -> NewWake {
    NewWake {
        target: req.parent,
        kind: WakeKind::ParentReport,
        payload: wake::payload(text, short, req.id),
        dedupe_key: dedupe_key(req.id, row),
        task,
        at,
    }
}

/// `parent:C-0193:<row id>`: new for every row, so each row wakes once.
pub fn dedupe_key(corr: CorrelationId, row: ParentRowId) -> String {
    format!("parent:{corr}:{row}")
}

/// A fact row, in the caller's transaction.
fn record_fact(
    tx: &Connection,
    notify: &mut Notify,
    event: &FactEvent,
    outcome: &facts::Outcome,
    tz: &TimeZone,
    at: Timestamp,
) -> Result<bool, StoreError> {
    if store::has_source_event(tx, event.id)? {
        return Ok(false);
    }
    let Some(req) = correlation::request(tx, event.correlation)? else {
        return Ok(false);
    };
    let cause = if outcome.kind == facts::OutcomeType::Started {
        StatusCause::TaskStarted
    } else {
        StatusCause::Fact
    };
    correlation::apply_status_rule(tx, req.id, req.status, cause)?;
    let inputs = store::snapshot_inputs(tx, req.id, facts::FACT_KINDS, Some(event.id))?;
    let snapshot = json("parent_channel.facts", &facts::snapshot(&inputs, tz))?;
    let row = store::insert(
        tx,
        &NewChannelRow {
            secondmate: req.secondmate,
            correlation: req.id,
            facts: &snapshot,
            judgment: None,
            at,
        },
    )?;
    let (text, short) = wake::fact_wake(req.secondmate, req.id, outcome);
    events::insert(
        tx,
        &NewEvent {
            kind: FACTS_EVENT,
            agent: Some(req.agent),
            task: Some(event.task),
            turn: None,
            summary: &text,
            payload: serde_json::json!({
                "correlation": req.id,
                "row": row,
                "type": outcome.kind,
                "source_event": event.id,
            }),
            at,
        },
    )?;
    Inbox::post(
        tx,
        &channel_wake(&req, row, Some(event.task), &text, &short, at),
        notify,
    )?;
    Ok(true)
}

/// A judgment row, in the caller's transaction.
fn record_judgment(
    tx: &Connection,
    notify: &mut Notify,
    req: &RequestFacts,
    judgment: &ParentJudgment,
    tz: &TimeZone,
    at: Timestamp,
) -> Result<ParentRowId, StoreError> {
    let summary = neutralise(&judgment.summary);
    let recommendation = judgment.recommendation.as_deref().map(neutralise);
    let needs_user = judgment.needs_user.as_deref().map(neutralise);
    let stored = serde_json::json!({
        "summary": summary,
        "recommendation": recommendation,
        "needs_user": needs_user,
    });
    correlation::apply_status_rule(tx, req.id, req.status, StatusCause::Report)?;
    let inputs = store::snapshot_inputs(tx, req.id, facts::FACT_KINDS, None)?;
    let snapshot_facts = facts::snapshot(&inputs, tz);
    let snapshot = json("parent_channel.facts", &snapshot_facts)?;
    let row = store::insert(
        tx,
        &NewChannelRow {
            secondmate: req.secondmate,
            correlation: req.id,
            facts: &snapshot,
            judgment: Some(&stored),
            at,
        },
    )?;
    let (text, short) = wake::judgment_wake(
        req.secondmate,
        req.id,
        &neutralise(&req.request),
        &summary,
        recommendation.as_deref(),
        needs_user.as_deref(),
        &snapshot_facts,
    );
    let line = crate::tasks::cut_chars(
        &format!("{} reported on {}: {summary}", req.secondmate, req.id),
        EVENT_SUMMARY_CHARS,
        "…",
    );
    events::insert(
        tx,
        &NewEvent {
            kind: REPORT_EVENT,
            agent: Some(req.agent),
            task: None,
            turn: None,
            summary: &line,
            payload: serde_json::json!({
                "correlation": req.id,
                "row": row,
                "judgment": stored,
            }),
            at,
        },
    )?;
    Inbox::post(tx, &channel_wake(req, row, None, &text, &short, at), notify)?;
    Ok(row)
}
