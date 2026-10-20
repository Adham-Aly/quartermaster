//! A Careful task's review pass, tied to the exact commit it reviewed: a pass counts only for that
//! head, and new commits need a re-review.
//!
//! [`passed_for`] is `verification::review::passed_for`: the head a pass is for is the
//! reviewed task's head when the verdict was given, recorded on its `task.review_passed` event
//! (after an app-driven re-review that is the new head, not the review task's base commit).

use qm_policy::merge_gate::TaskGate;
use qm_store::StoreError;
use qm_store::tasks as task_store;
use qm_types::domain::VerificationLevel;
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;

/// The commit the latest passed review of `task` was given on, as its event recorded it.
pub(crate) fn reviewed_head(
    conn: &Connection,
    task: TaskId,
) -> Result<Option<CommitSha>, StoreError> {
    Ok(
        task_store::last_event(conn, task, crate::verification::review::REVIEW_PASSED_EVENT)?
            .and_then(|(_, p)| {
                p.get("head")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|h| h.parse::<CommitSha>().ok())
            }),
    )
}

/// Whether `task`'s review passed for exactly `head`.
pub(crate) fn passed_for(
    conn: &Connection,
    task: TaskId,
    head: &CommitSha,
) -> Result<bool, StoreError> {
    crate::verification::review::passed_for(conn, task, head)
}

/// The refusal when the review passed on another commit.
pub(crate) fn stale_review(reviewed: Option<&CommitSha>) -> String {
    match reviewed {
        Some(sha) => format!(
            "the review passed on {}; new commits need a re-review",
            sha.short()
        ),
        None => "the review's commit can't be found; the head needs a re-review".to_owned(),
    }
}

/// The refusal when `task` is Careful and its review passed, but not on `head` (`None` when the
/// review doesn't apply or passed on exactly `head`; a review that never passed is the gate's).
pub(crate) fn review_refusal(
    conn: &Connection,
    task: TaskId,
    gate: TaskGate,
    head: &CommitSha,
) -> Result<Option<String>, StoreError> {
    if gate.level != Some(VerificationLevel::Reviewed) || !gate.review_passed {
        return Ok(None);
    }
    if passed_for(conn, task, head)? {
        return Ok(None);
    }
    Ok(Some(stale_review(reviewed_head(conn, task)?.as_ref())))
}
