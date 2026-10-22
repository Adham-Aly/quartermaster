//! The merge gate: when a task may push, finish and be merged.
//!
//! - "'Passed' means every check run and every commit status on that exact commit succeeded, and
//!   every required check is present."
//! - "Every current check must pass before the task finishes. A project with no CI checks counts as
//!   green when Verified names the local checks that ran on the final commit."
//! - `pr_open`: "Direct-PR mode only; never for answer-only tasks; refused under `reviewed`
//!   until the review passes; names checked." "Pushing a fix without declaring a diagnosis
//!   is refused by `pr_open`" ([`crate::repair::RepairState::on_push`]).
//! - `settle_task`: "Answer-only tasks can't land; review must have passed for `reviewed`;
//!   autonomy normal → held action; only the approved commit is merged."
//! - Merging under normal autonomy: "The app checks that the PR head is still `a1b2c3d`
//!   and that every required check passed on exactly that commit."
//! - Changing the level on a running task: "Raising it means the missing validation or
//!   review must happen before the next push or merge, even if a pull request is already open."

use qm_types::domain::{DeliveryMode, TaskKind, VerificationLevel};
use qm_types::error::Refusal;
use qm_types::ids::CommitSha;

/// One check run or commit status on a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Pending,
    Succeeded,
    /// Failed, errored, cancelled, timed out or anything else that isn't success.
    Failed,
}

/// A named check on the head commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Check<'a> {
    pub name: &'a str,
    pub state: CheckState,
}

/// Everything GitHub reports on one commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChecksOnCommit<'a> {
    pub runs: &'a [Check<'a>],
    pub statuses: &'a [Check<'a>],
    /// The branch protection's required check names.
    pub required: &'a [&'a str],
}

/// The checks' verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChecksVerdict {
    Passed,
    Pending,
    Failed(Vec<String>),
    MissingRequired(Vec<String>),
    /// No checks at all: the local-checks rule applies.
    NoChecks,
}

/// "Passed": every check run and commit status on the commit succeeded, and every required check
/// is present.
pub fn checks_verdict(c: &ChecksOnCommit<'_>) -> ChecksVerdict {
    let all = || c.runs.iter().chain(c.statuses.iter());
    if c.runs.is_empty() && c.statuses.is_empty() && c.required.is_empty() {
        return ChecksVerdict::NoChecks;
    }
    let failed: Vec<String> = all()
        .filter(|k| k.state == CheckState::Failed)
        .map(|k| k.name.to_owned())
        .collect();
    if !failed.is_empty() {
        return ChecksVerdict::Failed(failed);
    }
    if all().any(|k| k.state == CheckState::Pending) {
        return ChecksVerdict::Pending;
    }
    let missing: Vec<String> = c
        .required
        .iter()
        .filter(|r| !all().any(|k| k.name == **r))
        .map(|r| (*r).to_owned())
        .collect();
    if !missing.is_empty() {
        return ChecksVerdict::MissingRequired(missing);
    }
    ChecksVerdict::Passed
}

/// "CI green": passed, or no checks with Verified naming the local checks on the final commit.
pub fn check_green(verdict: &ChecksVerdict, local_checks_named: bool) -> Result<(), Refusal> {
    match verdict {
        ChecksVerdict::Passed => Ok(()),
        ChecksVerdict::NoChecks if local_checks_named => Ok(()),
        ChecksVerdict::NoChecks => Err(Refusal::new(
            "the project has no CI checks, so Verified must name the local checks run on the final commit",
        )),
        ChecksVerdict::Pending => Err(Refusal::new("checks are still running on the head commit")),
        ChecksVerdict::Failed(names) => Err(Refusal::new(format!(
            "checks failed on the head commit: {}",
            names.join(", ")
        ))),
        ChecksVerdict::MissingRequired(names) => Err(Refusal::new(format!(
            "required checks haven't reported on the head commit: {}",
            names.join(", ")
        ))),
    }
}

/// A task's verification state, as the gates read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskGate {
    pub kind: TaskKind,
    pub delivery: DeliveryMode,
    /// The level the task runs under (after Careful ships as standard off direct PR); `None`
    /// for tasks with no level.
    pub level: Option<VerificationLevel>,
    pub review_passed: bool,
    /// The level was raised and the missing validation hasn't been reported yet.
    pub validation_pending: bool,
    /// Any hold on the task (repair limit, review round limit, stuck, needs you).
    pub held: bool,
}

fn needs_review(g: TaskGate) -> bool {
    g.level == Some(VerificationLevel::Reviewed) && !g.review_passed
}

/// `pr_open`.
pub fn check_pr_open(g: &TaskGate) -> Result<(), Refusal> {
    if g.kind.is_answer_only() {
        return Err(Refusal::new(
            "answer-only tasks deliver an answer, never a pull request",
        ));
    }
    if g.delivery != DeliveryMode::DirectPr {
        return Err(Refusal::new(
            "pr_open works only in direct pull-request delivery",
        ));
    }
    if g.held {
        return Err(Refusal::new(
            "the task is held; the decision is with the user",
        ));
    }
    if needs_review(*g) {
        return Err(Refusal::new(
            "this task is Careful: commit locally and report; the pull request opens after the review passes",
        ));
    }
    if g.validation_pending {
        return Err(Refusal::new(
            "the verification level was raised: do the missing validation and report it before pushing",
        ));
    }
    Ok(())
}

/// A "done" report: the task may finish. `checks` is the verdict on the final head (ignored for
/// tasks with no level and for answer-only work).
pub fn check_finish(
    g: &TaskGate,
    checks: &ChecksVerdict,
    local_checks_named: bool,
) -> Result<(), Refusal> {
    if g.held {
        return Err(Refusal::new(
            "the task is held and can't finish until the user decides",
        ));
    }
    let Some(level) = g.level else {
        return Ok(());
    };
    if level == VerificationLevel::Reviewed && !g.review_passed {
        // "The builder commits locally and reports. It doesn't push or open the pull request":
        // that report goes to the review, and CI comes after the review passes.
        return Ok(());
    }
    if g.validation_pending {
        return Err(Refusal::new(
            "the verification level was raised; the missing validation comes first",
        ));
    }
    // Branch-only work isn't pushed until settling, so its verdict is usually `NoChecks` and the
    // local-checks rule applies.
    check_green(checks, local_checks_named)
}

/// `settle_task`'s gate for merging or landing code, before autonomy decides between merging and
/// holding.
pub fn check_mergeable(
    g: &TaskGate,
    checks: &ChecksVerdict,
    local_checks_named: bool,
) -> Result<(), Refusal> {
    if g.kind.is_answer_only() {
        return Err(Refusal::new("answer-only tasks can't land"));
    }
    if needs_review(*g) {
        return Err(Refusal::new(
            "the review must pass before this task can be merged",
        ));
    }
    if g.held {
        return Err(Refusal::new(
            "the task is held; the decision is with the user",
        ));
    }
    if g.validation_pending {
        return Err(Refusal::new(
            "the verification level was raised; the missing validation must happen before a merge",
        ));
    }
    if g.level.is_some() {
        check_green(checks, local_checks_named)?;
    }
    Ok(())
}

/// Your approval applies only at the approved commit: the head is unchanged and every required
/// check passed on exactly that commit.
pub fn check_approval(
    approved: &CommitSha,
    current_head: &CommitSha,
    checks_on_current: &ChecksVerdict,
    local_checks_named: bool,
) -> Result<(), Refusal> {
    if approved != current_head {
        return Err(Refusal::new(format!(
            "the branch moved from {} to {} since the approval; it needs a new approval",
            approved.short(),
            current_head.short()
        )));
    }
    check_green(checks_on_current, local_checks_named)
}

/// Every pull request the app merges is a squash merge, so a repository or ruleset that doesn't
/// allow squash merges stops it, with the reason. `repo_allows` is the repository's "Allow squash
/// merging" setting and `ruleset_methods` the methods the branch's rulesets allow; `None` is
/// unknown, which doesn't stop the merge (GitHub then refuses it itself).
pub fn check_squash_allowed(
    repo: &str,
    branch: &str,
    repo_allows: Option<bool>,
    ruleset_methods: Option<&[String]>,
) -> Result<(), Refusal> {
    if repo_allows == Some(false) {
        return Err(Refusal::new(format!(
            "{repo} doesn't allow squash merges, and the app merges every pull request by squash; turn on \"Allow squash merging\" in the repository's settings on GitHub, then settle again"
        )));
    }
    if let Some(methods) = ruleset_methods
        && !methods.iter().any(|m| m == "squash")
    {
        let allowed = if methods.is_empty() {
            "none".to_owned()
        } else {
            methods.join(", ")
        };
        return Err(Refusal::new(format!(
            "a ruleset on {repo}'s {branch} allows only these merge methods: {allowed}; the app merges every pull request by squash, so allow squash in that ruleset on GitHub, then settle again"
        )));
    }
    Ok(())
}

/// GitHub's own refusal of a merge, in words that say what to change when it's about squash
/// merges.
pub fn merge_refused_reason(pr: u64, message: &str) -> String {
    if message.to_lowercase().contains("squash") {
        format!(
            "GitHub won't squash-merge PR #{pr} ({message}); the app merges every pull request by squash, so allow squash merges for the repository and its rulesets on GitHub, then settle again"
        )
    } else {
        format!("GitHub won't merge PR #{pr}: {message}")
    }
}

#[cfg(test)]
mod squash_tests {
    use super::*;

    #[test]
    fn a_repository_without_squash_merges_is_refused_with_the_reason() {
        let refusal = check_squash_allowed("o/r", "main", Some(false), None).expect_err("refused");
        assert!(
            refusal.reason.contains("doesn't allow squash merges"),
            "{}",
            refusal.reason
        );
        assert!(
            refusal.reason.contains("Allow squash merging"),
            "{}",
            refusal.reason
        );
    }

    #[test]
    fn a_ruleset_without_squash_is_refused_with_the_methods_it_allows() {
        let methods = vec!["merge".to_owned(), "rebase".to_owned()];
        let refusal =
            check_squash_allowed("o/r", "main", Some(true), Some(&methods)).expect_err("refused");
        assert!(
            refusal.reason.contains("merge, rebase"),
            "{}",
            refusal.reason
        );
    }

    #[test]
    fn squash_allowed_or_unknown_goes_ahead() {
        let methods = vec!["squash".to_owned()];
        assert!(check_squash_allowed("o/r", "main", Some(true), Some(&methods)).is_ok());
        assert!(check_squash_allowed("o/r", "main", None, None).is_ok());
    }

    #[test]
    fn githubs_squash_refusal_says_what_to_change() {
        let reason = merge_refused_reason(4, "Squash merges are not allowed on this repository.");
        assert!(reason.contains("allow squash merges"), "{reason}");
        assert_eq!(
            merge_refused_reason(4, "Pull Request is not mergeable"),
            "GitHub won't merge PR #4: Pull Request is not mergeable"
        );
    }
}
