//! Fencing GitHub writes.
//!
//! `github_write` — "Comments, labels, issues" — "Outside its own project, or in an answer-only
//! task → held action." Crewmate processes can't push or write to GitHub themselves; "the
//! app's own pushes and merges run after its checks, in its own environment".

use qm_types::domain::TaskKind;

/// A repository on GitHub, `owner/name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repo<'a> {
    pub owner: &'a str,
    pub name: &'a str,
}

impl Repo<'_> {
    /// GitHub names compare case-insensitively.
    pub fn same_as(&self, other: &Repo<'_>) -> bool {
        self.owner.eq_ignore_ascii_case(other.owner) && self.name.eq_ignore_ascii_case(other.name)
    }
}

/// What happens to a `github_write` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteDecision {
    /// Inside the crewmate's own project, in a task that can change code: the app writes.
    Write,
    /// Held for your approval, with the reason.
    Hold(&'static str),
}

/// The fence. `project_repo` is `None` when the task's project has no GitHub remote, so every
/// write is outside it.
pub fn write_decision(
    kind: TaskKind,
    project_repo: Option<Repo<'_>>,
    target: Repo<'_>,
) -> WriteDecision {
    if kind.is_answer_only() {
        return WriteDecision::Hold("GitHub writes from an answer-only task wait for the user");
    }
    match project_repo {
        Some(own) if own.same_as(&target) => WriteDecision::Write,
        _ => WriteDecision::Hold("GitHub writes outside the task's own project wait for the user"),
    }
}
