//! A task's life as one table. Pure: [`next`] is the table, and settling calls it too.
//!
//! `queued→preparing→running⇄waiting·held·stuck→reported→in review→settling→landed→closed`, plus
//! the side exits `failed`, `stopped` and `discarded`, which are transient: a task in one of them
//! always has a retirement in progress, and the app finishes it (at startup too), so every exit
//! retires the worktree the same way (`tasks::close::retire`).

use qm_policy::delivery::Deliverable;
use qm_types::domain::{TaskOutcome, TaskStatus};
use qm_types::error::Refusal;

/// Something that moves a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskEvent {
    /// Routed and settled: phase C begins.
    Prepared,
    /// The first turn started with the brief.
    BriefSent,
    /// `declare_wait` or `ask_decision`.
    Waited,
    /// The waited-on task finished, or an outside wait's next turn started.
    WaitOver,
    /// The app held it for your decision.
    Held,
    /// Your answer on the card released it.
    Released,
    /// The idle ladder's last step.
    Stuck,
    /// Your Continue on the stuck notice.
    Continued,
    /// A clear `report`, done or blocked.
    Reported,
    /// A `needs_decision` report.
    NeedsDecision,
    /// The supervisor's message after a decision question or a report.
    Steered,
    /// A review task building on it started.
    ReviewStarted,
    /// The review passed.
    ReviewPassed,
    /// `settle_task` accepted.
    SettleStarted,
    /// The merge or push failed, or you rejected it.
    SettleRefused,
    /// The merge, delivery or landing is a fact.
    Landed,
    Failed,
    Stopped,
    Discarded,
    Closed,
}

/// Whether the task is open: everything but `closed`.
pub const fn is_open(status: TaskStatus) -> bool {
    !matches!(status, TaskStatus::Closed)
}

/// Whether the task is ending by a side exit, its retirement under way.
pub const fn is_exiting(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Failed | TaskStatus::Stopped | TaskStatus::Discarded
    )
}

/// Whether the task has (or should have) a worktree: past preparing, not closed. A preparing task
/// may have one from an interrupted attempt; reconcile counts it too.
pub const fn has_worktree(status: TaskStatus) -> bool {
    !matches!(status, TaskStatus::Queued | TaskStatus::Closed)
}

/// Whether a task in `status` counts as finished for a task waiting on it or blocked by it:
/// `reported`, `landed` or `closed`.
pub const fn is_finished(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Reported | TaskStatus::Landed | TaskStatus::Closed
    )
}

/// The status `event` moves a task in `from` to, or a refusal naming both statuses.
pub fn next(from: TaskStatus, event: TaskEvent) -> Result<TaskStatus, Refusal> {
    use TaskEvent as E;
    use TaskStatus as S;
    let to = match (from, event) {
        (S::Queued, E::Prepared) => Some(S::Preparing),
        (S::Preparing, E::BriefSent)
        | (S::Waiting, E::WaitOver | E::Steered)
        | (S::Held, E::Released)
        | (S::Stuck, E::Continued)
        | (S::InReview, E::ReviewPassed)
        // From `in_review` (the review's findings passed to the builder, or its level lowered)
        // and `stuck` (a steer or your direct message to a stuck crewmate) too.
        | (S::Reported | S::InReview | S::Stuck, E::Steered) => Some(S::Running),
        (S::Running, E::Waited | E::NeedsDecision) => Some(S::Waiting),
        // The review round limit, or a blocked review, holds the builder in review. The repair
        // limit can hold a branch-only task settling on its pushed branch's checks.
        (S::Running | S::Waiting | S::Reported | S::InReview | S::Settling, E::Held) => {
            Some(S::Held)
        }
        (S::Running, E::Stuck) => Some(S::Stuck),
        (S::Running | S::Waiting, E::Reported) | (S::Settling, E::SettleRefused) => {
            Some(S::Reported)
        }
        // A re-report replaces the report that stands, carrying the new head; from `settling` only
        // while its approval is still pending (the report voids it; a merge or push under way
        // refuses it, `tasks::report`).
        (S::Reported | S::Settling, E::Reported) => Some(S::Reported),
        (S::Reported, E::ReviewStarted) => Some(S::InReview),
        (S::Reported | S::Running, E::SettleStarted) => Some(S::Settling),
        (S::Settling, E::Landed) => Some(S::Landed),
        (s, E::Failed) if is_open(s) && !is_exiting(s) => Some(S::Failed),
        (s, E::Stopped) if is_open(s) && !is_exiting(s) => Some(S::Stopped),
        (s, E::Discarded) if is_open(s) && !is_exiting(s) => Some(S::Discarded),
        (S::Landed | S::Reported | S::Failed | S::Stopped | S::Discarded, E::Closed) => {
            Some(S::Closed)
        }
        _ => None,
    };
    to.ok_or_else(|| {
        Refusal::new(format!(
            "the task is {from}; it can't go to {} from there",
            target_word(event)
        ))
    })
}

/// [`next`] with the task named in the refusal ("T-0042 is reported; it can't go to held from
/// there").
pub fn next_for(
    task: qm_types::ids::TaskId,
    from: TaskStatus,
    event: TaskEvent,
) -> Result<TaskStatus, Refusal> {
    next(from, event).map_err(|_| {
        Refusal::new(format!(
            "{task} is {from}; it can't go to {} from there",
            target_word(event)
        ))
    })
}

/// The status an event aims at, for the refusal.
const fn target_word(event: TaskEvent) -> &'static str {
    match event {
        TaskEvent::Prepared => "preparing",
        TaskEvent::BriefSent
        | TaskEvent::WaitOver
        | TaskEvent::Released
        | TaskEvent::Continued
        | TaskEvent::Steered
        | TaskEvent::ReviewPassed => "running",
        TaskEvent::Waited | TaskEvent::NeedsDecision => "waiting",
        TaskEvent::Held => "held",
        TaskEvent::Stuck => "stuck",
        TaskEvent::Reported | TaskEvent::SettleRefused => "reported",
        TaskEvent::ReviewStarted => "in_review",
        TaskEvent::SettleStarted => "settling",
        TaskEvent::Landed => "landed",
        TaskEvent::Failed => "failed",
        TaskEvent::Stopped => "stopped",
        TaskEvent::Discarded => "discarded",
        TaskEvent::Closed => "closed",
    }
}

/// The outcome a close records for a task ending from `exit` (its status just before `closed`).
/// `delivered` is what its newest done report delivered.
pub fn outcome_for(exit: TaskStatus, delivered: Deliverable) -> TaskOutcome {
    match exit {
        TaskStatus::Failed => TaskOutcome::Failed,
        TaskStatus::Stopped => TaskOutcome::Stopped,
        TaskStatus::Discarded => TaskOutcome::Discarded,
        TaskStatus::Reported if delivered == Deliverable::Answer => TaskOutcome::Answered,
        _ => TaskOutcome::Landed,
    }
}
