//! Settling: landing finished work under each delivery mode, at exactly the approved commit.
//!
//! The merge-gate invariants are enforced here and in `qm_git::land`:
//!
//! - [`settle`]: `settle_task`: who and what, blockers, names, the agreed head, the gate, the plan,
//!   and either one held action written once or the work done now.
//! - [`land_pr`]: merging a pull request: re-verified on GitHub at approval time, the SHA
//!   precondition, the read-back, no blind retries.
//! - [`push`]: branch-only: a push with a lease, never a merge; delivered when the checks on the
//!   pushed branch allow it.
//! - [`in_place`]: build in place, into a local repository's checkout.
//! - [`facts`]: the fact, the supervisor's `settled` wake, the parent channel.
//! - [`performers`]: carrying out your Approve or Reject, and GitHub writes.
//! - [`cards`]: the approval cards.
//! - [`recover`]: startup, and merges GitHub answered late or unclearly: never a merge without a
//!   read-back.
//! - `review`: a Careful task's review pass counts only for the exact head it reviewed.

pub mod cards;
pub mod facts;
pub mod in_place;
pub mod land_pr;
pub mod performers;
pub mod push;
pub mod recover;
pub(crate) mod review;
pub mod settle;

use crate::delivery::DeliveryService;
use crate::inbox::Inbox;
use crate::tasks::lifecycle::{TaskEvent, next};
use crate::tasks::{TaskService, new_wake};
use jiff::Timestamp;
use qm_store::StoreError;
use qm_store::tasks as task_store;
use qm_tools::defs::SettleOutcome;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{TaskStatus, WakeKind};
use qm_types::ids::{CommitSha, HeldActionId, TaskId};
use std::fmt;

pub use facts::Fact;

/// `events.kind` of a hold written for your approval (its payload carries the card's facts).
pub const SETTLE_HELD_EVENT: &str = "settle.held";
/// `events.kind` committed before the merge request goes to GitHub.
pub const MERGE_REQUESTED_EVENT: &str = "settle.merge_requested";
/// `events.kind` of a settle that didn't land, with the reason.
pub const SETTLE_REFUSED_EVENT: &str = "settle.refused";
/// `events.kind` of a branch-only push.
pub const SETTLE_PUSHED_EVENT: &str = "settle.pushed";
/// `events.kind` of a merge whose outcome GitHub didn't confirm yet (late or unclear): not a
/// conclusion; the merge is read again until GitHub settles it.
pub const MERGE_UNKNOWN_EVENT: &str = "settle.merge_unknown";
/// `events.kind` of a merge request concluded by its read-back after the task had left settling
/// (merged late, or not merged); the supervisor is woken with it.
pub const MERGE_CONCLUDED_EVENT: &str = "settle.merge_concluded";

/// Who asked for the landing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `settle_task` with the project's autonomy auto at that moment.
    Auto,
    /// Your Approve on the card.
    Approval { held: HeldActionId },
}

/// What a landing came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landing {
    Landed(Fact),
    /// Branch-only: pushed; it lands when the checks on the pushed branch allow it.
    Pushed {
        branch: String,
        head: CommitSha,
    },
    /// GitHub answered the merge late or unclearly: its outcome is unknown, the task stays
    /// settling, and the merge is read again until GitHub confirms it.
    Unknown {
        reason: String,
    },
    /// Not landed, with the reason; `superseded` when the branch moved since the approval.
    Refused {
        reason: String,
        superseded: bool,
    },
}

/// Something went wrong in the app itself (not a rule): nothing was merged or pushed.
#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Failed(String),
}

/// Settling: `settle_task`, the performers and recovery. Cheap to clone.
#[derive(Clone, Debug)]
pub struct SettlingService {
    delivery: DeliveryService,
}

impl SettlingService {
    pub fn new(delivery: DeliveryService) -> Self {
        Self { delivery }
    }

    pub(crate) fn delivery(&self) -> &DeliveryService {
        &self.delivery
    }

    pub(crate) fn tasks(&self) -> &TaskService {
        self.delivery.tasks()
    }

    /// `settle_task`.
    pub async fn settle_task(
        &self,
        caller: &Caller,
        task: TaskId,
    ) -> Result<SettleOutcome, ToolFailure> {
        settle::settle(self, caller, task).await
    }

    /// Startup: concludes every settle the last run left half-done. Awaited before the seat runs
    /// and before any card can be answered.
    pub async fn recover(&self) {
        recover::run(self).await;
    }

    /// A landing that didn't happen, in one transaction: the task `settling → reported` (only if
    /// it's still settling), `settle.refused` with the reason, and under autonomy the supervisor's
    /// `settle_failed` wake. Nothing is retried with another SHA.
    pub(crate) async fn refuse(
        &self,
        task: TaskId,
        head: &CommitSha,
        reason: &str,
        origin: Origin,
        wake_text: Option<String>,
    ) -> Result<(), StoreError> {
        let at = self.delivery.clock().now();
        let head = head.clone();
        let reason = reason.to_owned();
        self.tasks()
            .write(move |tx, notify| {
                refuse_in(tx, notify, task, &head, &reason, origin, wake_text, at).map(drop)
            })
            .await
    }
}

/// [`SettlingService::refuse`] inside the caller's transaction. Returns whether the task moved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn refuse_in(
    tx: &rusqlite::Connection,
    notify: &mut crate::inbox::Notify,
    task: TaskId,
    head: &CommitSha,
    reason: &str,
    origin: Origin,
    wake_text: Option<String>,
    at: Timestamp,
) -> Result<bool, StoreError> {
    let Some(row) = task_store::load(tx, task)? else {
        return Ok(false);
    };
    let moved = row.status == TaskStatus::Settling;
    if moved && let Ok(to) = next(row.status, TaskEvent::SettleRefused) {
        task_store::set_status(tx, task, row.status, to, at)?;
    }
    let held = match origin {
        Origin::Approval { held } => Some(held),
        Origin::Auto => None,
    };
    let event = task_store::event(
        tx,
        SETTLE_REFUSED_EVENT,
        task,
        None,
        &format!("{task} not settled: {reason}"),
        serde_json::json!({ "reason": reason, "head": head, "held": held }),
        at,
    )?;
    if origin == Origin::Auto || wake_text.is_some() {
        let text = wake_text.unwrap_or_else(|| {
            format!("Settling {task} failed: {reason}. Nothing was merged; it's back to reported.")
        });
        Inbox::post(
            tx,
            &new_wake(
                row.supervisor,
                WakeKind::SettleFailed,
                task,
                format!("settle_failed:{task}:{}:{event}", head.short()),
                &text,
                &format!("{task} not settled"),
                serde_json::json!({ "head": head }),
                at,
            ),
            notify,
        )?;
    }
    Ok(moved)
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Approval { held } => write!(f, "approval of {held}"),
        }
    }
}
