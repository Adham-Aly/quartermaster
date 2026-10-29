//! Fencing GitHub writes.
//!
//! `github_write` — "Comments, labels, issues" — "Outside its own project → held action." A
//! crewmate also has the user's own `gh` login; `github_write` and its holds are the in-band path
//! its prompt makes the default, not the only one.

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
    /// Inside the crewmate's own project: the app writes.
    Write,
    /// Held for your approval, with the reason.
    Hold(&'static str),
}

/// Whether the app writes or holds. `project_repo` is `None` when the task's project has no GitHub
/// remote, so every write is outside it.
pub fn write_decision(project_repo: Option<Repo<'_>>, target: Repo<'_>) -> WriteDecision {
    match project_repo {
        Some(own) if own.same_as(&target) => WriteDecision::Write,
        _ => WriteDecision::Hold("GitHub writes outside the task's own project wait for the user"),
    }
}
