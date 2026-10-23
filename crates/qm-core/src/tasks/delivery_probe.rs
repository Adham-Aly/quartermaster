//! Whether a task can deliver, checked before anything starts and answered to the crewmate by
//! `delivery_check`, from the same probe so the two never disagree.
//!
//! [`LocalDeliveryProbe`] knows what it can without GitHub: `origin` exists and points at GitHub,
//! and the GitHub CLI is logged in (the setup check's facts). Delivery's `GithubDeliveryProbe`
//! wraps it and adds what needs the API.

use super::ProjectRow;
use crate::setup_check::SetupFacts;
use qm_git::{Git, ORIGIN, Repo};
use qm_tools::defs::{Capability, DeliveryCheck};
use qm_types::BoxFuture;
use qm_types::domain::DeliveryMode;
use std::fmt;
use std::path::Path;
use tokio::sync::watch;

/// What delivery would need, and the gaps in plain sentences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryReadiness {
    pub push: Capability,
    pub pull_request: Capability,
    pub checks: Capability,
    pub gaps: Vec<String>,
}

impl DeliveryReadiness {
    /// Everything works.
    pub fn ready() -> Self {
        Self {
            push: Capability::Ok,
            pull_request: Capability::Ok,
            checks: Capability::Ok,
            gaps: Vec::new(),
        }
    }

    /// Whether any gap stands.
    pub fn has_gaps(&self) -> bool {
        !self.gaps.is_empty()
    }
}

/// Probes a project's delivery path.
pub trait DeliveryProbe: Send + Sync + fmt::Debug {
    fn probe<'a>(
        &'a self,
        project: &'a ProjectRow,
        repo_root: &'a Path,
    ) -> BoxFuture<'a, DeliveryReadiness>;

    /// Whether `branch` already exists on the project's GitHub repository: `Some(true)` or
    /// `Some(false)` when GitHub answered, `None` when it can't be asked (no GitHub remote, or
    /// GitHub unreadable). A task's branch name is checked with it before the branch is made.
    fn remote_branch_taken<'a>(
        &'a self,
        project: &'a ProjectRow,
        branch: &'a str,
    ) -> BoxFuture<'a, Option<bool>> {
        let _ = (project, branch);
        Box::pin(async { None })
    }
}

/// The probe that needs no GitHub API.
#[derive(Clone, Debug)]
pub struct LocalDeliveryProbe {
    git: Git,
    setup: watch::Receiver<SetupFacts>,
}

impl LocalDeliveryProbe {
    pub fn new(git: Git, setup: watch::Receiver<SetupFacts>) -> Self {
        Self { git, setup }
    }

    /// The GitHub CLI's login, as the setup check last found it: `Some(true)` logged in,
    /// `Some(false)` missing or logged out, `None` before the first check has run. An unknown
    /// login is no gap: the GitHub probe checks GitHub access itself.
    fn gh_logged_in(&self) -> Option<bool> {
        let facts = self.setup.borrow();
        facts.checked.then(|| facts.gh.is_logged_in())
    }
}

impl DeliveryProbe for LocalDeliveryProbe {
    fn probe<'a>(
        &'a self,
        project: &'a ProjectRow,
        repo_root: &'a Path,
    ) -> BoxFuture<'a, DeliveryReadiness> {
        Box::pin(async move {
            let mut out = DeliveryReadiness::ready();
            let git = self.git.clone();
            let root = repo_root.to_owned();
            let origin = tokio::task::spawn_blocking(move || {
                Repo::open(git, &root)
                    .and_then(|repo| repo.remotes())
                    .map(|remotes| remotes.iter().any(|r| r == ORIGIN))
            })
            .await;
            match origin {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    out.push = Capability::Missing;
                    out.gaps.push(format!(
                        "{} has no remote named origin, so nothing can be pushed",
                        project.name
                    ));
                }
                Ok(Err(e)) => {
                    out.push = Capability::Missing;
                    out.gaps
                        .push(format!("{}'s repository can't be read: {e}", project.name));
                }
                Err(e) => {
                    out.push = Capability::Missing;
                    out.gaps.push(format!(
                        "{}'s repository couldn't be checked: {e}",
                        project.name
                    ));
                }
            }
            if out.push == Capability::Ok && project.github_remote.is_none() {
                out.push = Capability::Missing;
                out.pull_request = Capability::Missing;
                out.checks = Capability::Missing;
                out.gaps.push(format!(
                    "{}'s origin isn't on GitHub, so a pull request can't be opened",
                    project.name
                ));
            }
            if self.gh_logged_in() == Some(false) {
                out.pull_request = Capability::Missing;
                out.checks = Capability::Missing;
                out.gaps.push(
                    "GitHub CLI isn't logged in on this Mac; the setup check can fix that"
                        .to_owned(),
                );
            }
            out
        })
    }
}

/// `delivery_check`'s answer for a project's delivery mode: what isn't needed under the mode says
/// so, the rest is probed.
pub(crate) fn delivery_check(
    delivery: DeliveryMode,
    has_github_remote: bool,
    probed: Option<DeliveryReadiness>,
) -> DeliveryCheck {
    let not_needed = DeliveryCheck {
        push: Capability::NotNeeded,
        pull_request: Capability::NotNeeded,
        checks: Capability::NotNeeded,
        gaps: Vec::new(),
    };
    match (delivery, probed) {
        (DeliveryMode::BuildInPlace | DeliveryMode::Folder, _) | (_, None) => not_needed,
        (DeliveryMode::BranchOnly, _) if !has_github_remote => not_needed,
        (DeliveryMode::BranchOnly, Some(p)) => DeliveryCheck {
            push: p.push,
            pull_request: Capability::NotNeeded,
            checks: p.checks,
            gaps: p.gaps,
        },
        (DeliveryMode::DirectPr, Some(p)) => DeliveryCheck {
            push: p.push,
            pull_request: p.pull_request,
            checks: p.checks,
            gaps: p.gaps,
        },
    }
}
