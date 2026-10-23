//! What delivery and settling need from verification and from secondmates, as traits with safe
//! defaults, so delivery never calls verification's code directly; verification implements
//! [`VerificationPort`].

use crate::inbox::Notify;
use crate::settling::facts::Fact;
use jiff::Timestamp;
use qm_policy::merge_gate::TaskGate;
use qm_store::StoreError;
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;
use std::fmt;

/// What delivery and settling need from verification.
pub trait VerificationPort: Send + Sync + fmt::Debug {
    /// The gate facts for `qm_policy::merge_gate`: the level as the task runs it, review
    /// passed, validation pending, held.
    fn gate(&self, conn: &Connection, task: TaskId) -> Result<TaskGate, StoreError>;

    /// `pr_open` and a branch-only push, before pushing: the declared-diagnosis rule and the
    /// raised-level rule, checked only (nothing changes until the push is recorded).
    /// `body_complete`: the PR body has a Summary and a non-blank Testing section (or, for a
    /// branch-only push, the done report has the five headings).
    fn before_push(
        &self,
        conn: &Connection,
        task: TaskId,
        failing_now: &[String],
        body_complete: bool,
    ) -> Result<Result<(), Refusal>, StoreError>;

    /// In the push's transaction: what the push uses up. `failing_before`: the checks failing on
    /// the previously pushed head, as [`Self::before_push`] was given them.
    fn pushed(
        &self,
        conn: &Connection,
        task: TaskId,
        head: &CommitSha,
        failing_before: &[String],
        body_complete: bool,
    ) -> Result<(), StoreError>;

    /// In the final verdict's transaction: repair counting; may hold the task.
    fn checks_final(
        &self,
        conn: &Connection,
        task: TaskId,
        head: &CommitSha,
        failing: &[String],
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<ChecksFollowUp, StoreError>;
}

/// What verification adds to a final checks verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChecksFollowUp {
    /// Appended to the crewmate's `checks` text ("Repair attempts used: 2 of 3.").
    pub note: Option<String>,
    /// The task is held now (the repair limit).
    pub held: bool,
}

/// The secondmates' hook: a settled task's fact goes up the parent channel when a secondmate owns
/// it. Called in the transaction that records the fact.
pub trait ParentPort: Send + Sync + fmt::Debug {
    fn task_settled(&self, conn: &Connection, task: TaskId, fact: &Fact) -> Result<(), StoreError>;
}
