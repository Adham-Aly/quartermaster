//! The delivery probe with GitHub's API: the local probe first, then, when push, pull request
//! and checks look possible locally, what only the API can tell. `delivery_check` and the start
//! gate share it, so they never disagree.

use crate::tasks::ProjectRow;
use crate::tasks::delivery_probe::{DeliveryProbe, DeliveryReadiness, LocalDeliveryProbe};
use qm_github::{GithubClient, RepoRef};
use qm_tools::defs::Capability;
use qm_types::BoxFuture;
use std::path::Path;

/// [`LocalDeliveryProbe`] plus the API's answers.
#[derive(Clone, Debug)]
pub struct GithubDeliveryProbe {
    local: LocalDeliveryProbe,
    github: GithubClient,
}

impl GithubDeliveryProbe {
    pub fn new(local: LocalDeliveryProbe, github: GithubClient) -> Self {
        Self { local, github }
    }

    /// The probe over your `git` and the setup check's facts, as the app wires it.
    pub fn for_app(
        setup: tokio::sync::watch::Receiver<crate::setup_check::SetupFacts>,
        github: GithubClient,
    ) -> Self {
        Self::new(LocalDeliveryProbe::new(qm_git::Git::new(), setup), github)
    }
}

impl DeliveryProbe for GithubDeliveryProbe {
    fn remote_branch_taken<'a>(
        &'a self,
        project: &'a ProjectRow,
        branch: &'a str,
    ) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            let repo = project
                .github_remote
                .as_deref()
                .and_then(RepoRef::from_project)?;
            match self.github.branch_head(&repo, branch).await {
                Ok(head) => Some(head.is_some()),
                Err(e) => {
                    tracing::info!(%repo, error = %e.plain(), "GitHub couldn't be asked about a branch name");
                    None
                }
            }
        })
    }

    fn probe<'a>(
        &'a self,
        project: &'a ProjectRow,
        repo_root: &'a Path,
    ) -> BoxFuture<'a, DeliveryReadiness> {
        Box::pin(async move {
            let mut out = self.local.probe(project, repo_root).await;
            let all_ok = out.push == Capability::Ok
                && out.pull_request == Capability::Ok
                && out.checks == Capability::Ok;
            let Some(repo) = project
                .github_remote
                .as_deref()
                .and_then(RepoRef::from_project)
            else {
                return out;
            };
            if !all_ok {
                return out;
            }
            let info = match self.github.repo(&repo).await {
                Ok(info) => info,
                Err(e) if e.is_transient() => {
                    // A blip isn't a gap: the local answer stands, and pr_open says so if it
                    // persists.
                    tracing::info!(%repo, error = %e.plain(), "GitHub couldn't be asked about delivery");
                    return out;
                }
                Err(e) => {
                    out.push = Capability::Missing;
                    out.pull_request = Capability::Missing;
                    out.checks = Capability::Missing;
                    out.gaps.push(format!(
                        "{repo} can't be read with your GitHub login: {}",
                        e.plain()
                    ));
                    return out;
                }
            };
            if !info.can_push {
                out.push = Capability::Missing;
                out.pull_request = Capability::Missing;
                out.gaps
                    .push(format!("Your GitHub login can't push to {repo}"));
            } else if info.archived {
                out.push = Capability::Missing;
                out.pull_request = Capability::Missing;
                out.gaps.push(format!("{repo} is archived"));
            }
            match self
                .github
                .can_read_checks(&repo, &info.default_branch)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    out.checks = Capability::Missing;
                    out.gaps.push(format!(
                        "Check results on {repo} can't be read with your GitHub login"
                    ));
                }
                Err(e) => {
                    tracing::info!(%repo, error = %e.plain(), "GitHub couldn't be asked about checks");
                }
            }
            out
        })
    }
}
