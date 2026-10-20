//! The independent review's verdicts and round limit.
//!
//! "The review report opens with `verdict: pass|findings|blocked` … A report with no verdict
//! records nothing, and info-only findings count as a pass." "After a fix, the reviewer re-reviews
//! the new head." "After 2 re-reviews, a third 'findings' verdict holds the task and puts it to you
//! as one decision with the fixed choices below." "A pass releases the hold." "The review's risk
//! rating is recorded but never blocks a merge."

use qm_types::domain::{ReviewVerdict, Severity};

/// Two re-reviews; the third "findings" verdict (the first review plus two re-reviews) holds.
pub const MAX_RE_REVIEWS: u8 = 2;

/// What a review report does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// No verdict: records nothing.
    NotRecorded,
    /// A pass (or info-only findings): the builder pushes and opens the pull request.
    Pass,
    /// Findings: back to the builder, then a re-review of the new head.
    Findings,
    /// The round limit: hold the task as one decision.
    HoldRoundLimit,
    /// The reviewer is blocked: hold the task as one decision for you.
    ///
    /// A `blocked` verdict holds the task ("any blocked crewmate whose next move only you can
    /// make").
    HoldBlocked,
}

/// Interprets one review report. `re_reviews_done` counts re-reviews before this one (0 for the
/// first review).
pub fn outcome(
    verdict: Option<ReviewVerdict>,
    findings: &[Severity],
    re_reviews_done: u8,
) -> ReviewOutcome {
    match verdict {
        None => ReviewOutcome::NotRecorded,
        Some(ReviewVerdict::Pass) => ReviewOutcome::Pass,
        Some(ReviewVerdict::Blocked) => ReviewOutcome::HoldBlocked,
        Some(ReviewVerdict::Findings) if findings.iter().all(|s| *s == Severity::Info) => {
            ReviewOutcome::Pass
        }
        Some(ReviewVerdict::Findings) if re_reviews_done >= MAX_RE_REVIEWS => {
            ReviewOutcome::HoldRoundLimit
        }
        Some(ReviewVerdict::Findings) => ReviewOutcome::Findings,
    }
}
