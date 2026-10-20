//! Where a task's branch starts.
//!
//! The rules, enforced here from facts the core supplies:
//!
//! - **Default.** With no branch named anywhere, the task starts from `origin/<default branch>`,
//!   freshly fetched; the default branch is asked of the remote itself. If finding it or fetching
//!   fails, the task is refused with the reason rather than built on stale code. A project with
//!   no remote starts from its local default branch.
//! - **A typed base branch.** The task starts from `origin/<base_branch>`, freshly fetched, and
//!   its pull request targets that branch. It's refused unless it traces back to your own
//!   message, when the project has no remote, when the remote lacks the branch, or when the
//!   project's delivery isn't a pull request.
//!
//! *Whether* a branch traces back to you, and the project's delivery mode, are the core's facts
//! (who may supply a branch); this module applies the rule to them.

use qm_types::domain::DeliveryMode;
use qm_types::ids::CommitSha;

use crate::error::{BaseRefusal, GitError};
use crate::git::{GitOutput, Invocation};
use crate::network::Credentials;
use crate::repo::{ORIGIN, Repo, unexpected};

/// What the task asked to start from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseRequest {
    /// No branch named anywhere: the remote's default branch, freshly fetched.
    Default,
    /// A typed `base_branch`, from the Quartermaster, a routed request, a secondmate's standing
    /// branch or a task's recorded base (for retries, reviews and cleanup, which reuse it).
    Typed {
        /// The branch name on the remote (`release/2.4`); a leading `origin/` or `refs/heads/`
        /// is taken as naming the same branch.
        branch: String,
        /// Whether the core traced this branch back to your own message (a
        /// branch recorded on a task when it started was traced then, and stays so).
        traced_to_user: bool,
    },
}

/// Which base-branch rule a settled base came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BaseSource {
    /// The remote's default branch, freshly fetched.
    RemoteDefault,
    /// A typed base branch that traces back to you, freshly fetched.
    Typed,
    /// A project with no remote: its local default branch.
    LocalDefault,
}

/// A settled base: exactly where the task's branch starts, recorded on the task so it can't change
/// after the task starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledBase {
    /// The rule it came from.
    pub source: BaseSource,
    /// The branch it names, without a remote (`main`, `release/2.4`).
    pub branch: String,
    /// The remote it was fetched from (`origin`), or `None` for a project with no remote.
    pub remote: Option<String>,
    /// The ref as the brief shows it (`origin/main`, or `main` with no remote); `tasks.base_ref`.
    pub base_ref: String,
    /// The exact commit the task's branch starts at; `tasks.base_commit`.
    pub commit: CommitSha,
}

impl SettledBase {
    /// The typed base branch to record on the task (`tasks.base_branch`), or `None` for a
    /// default base.
    pub fn typed_branch(&self) -> Option<&str> {
        (self.source == BaseSource::Typed).then_some(self.branch.as_str())
    }

    /// The branch a pull request from this task targets: the typed branch, or the remote's
    /// default branch. `None` for a project with no remote, which can't open one.
    pub fn pull_request_target(&self) -> Option<&str> {
        self.remote.as_ref().map(|_| self.branch.as_str())
    }

    /// The full remote-tracking or local ref the base was read from
    /// (`refs/remotes/origin/main`, or `refs/heads/main`).
    pub fn full_ref(&self) -> String {
        match &self.remote {
            Some(remote) => format!("refs/remotes/{remote}/{}", self.branch),
            None => format!("refs/heads/{}", self.branch),
        }
    }
}

impl Repo {
    /// Settles where a task starts under the base-branch rule, fetching from `origin` as needed.
    ///
    /// `delivery` is the project's delivery mode and `local_default_branch` its recorded default
    /// branch (`projects.default_branch`), used only when the project has no remote.
    ///
    /// Refusals come back as [`GitError::BaseRefused`] with the reason; the task must not start.
    /// Other errors (git missing, an unreadable repository) are failures of the app, not refusals.
    /// The fetch updates only `refs/remotes/origin/<branch>`; your checkout, branches and
    /// `FETCH_HEAD` are untouched.
    pub fn settle_base(
        &self,
        request: &BaseRequest,
        delivery: DeliveryMode,
        local_default_branch: &str,
    ) -> Result<SettledBase, GitError> {
        match request {
            BaseRequest::Default => self.settle_default(local_default_branch),
            BaseRequest::Typed {
                branch,
                traced_to_user,
            } => self.settle_typed(branch, *traced_to_user, delivery),
        }
    }

    /// The remote's default branch, asked of the remote itself (`git ls-remote --symref`).
    /// Errors are base-branch refusals ([`BaseRefusal::DefaultBranchUnknown`]).
    pub fn remote_default_branch(&self, remote: &str) -> Result<String, GitError> {
        let out = self
            .network_command(
                remote,
                None,
                Credentials::Yours,
                ["ls-remote", "--symref", remote, "HEAD"],
            )
            .and_then(Invocation::output)
            .map_err(|e| {
                refusal_for_network_error(e, |detail| BaseRefusal::DefaultBranchUnknown {
                    remote: remote.to_owned(),
                    detail,
                })
            })?;
        if !out.success() {
            return Err(BaseRefusal::DefaultBranchUnknown {
                remote: remote.to_owned(),
                detail: out.stderr,
            }
            .into());
        }
        out.stdout
            .lines()
            .find_map(|line| {
                let (target, name) = line.strip_prefix("ref: ")?.split_once('\t')?;
                (name == "HEAD").then_some(target)
            })
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .filter(|branch| !branch.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                BaseRefusal::DefaultBranchUnknown {
                    remote: remote.to_owned(),
                    detail: "the remote didn't name its default branch".to_owned(),
                }
                .into()
            })
    }

    fn settle_default(&self, local_default_branch: &str) -> Result<SettledBase, GitError> {
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return self.settle_local_default(local_default_branch);
        }
        if !remotes.iter().any(|r| r == ORIGIN) {
            return Err(BaseRefusal::NoOriginRemote { remotes }.into());
        }
        let branch = self.remote_default_branch(ORIGIN)?;
        let commit = self.fetch_branch(ORIGIN, &branch)?;
        Ok(SettledBase {
            source: BaseSource::RemoteDefault,
            base_ref: format!("{ORIGIN}/{branch}"),
            remote: Some(ORIGIN.to_owned()),
            branch,
            commit,
        })
    }

    fn settle_local_default(&self, branch: &str) -> Result<SettledBase, GitError> {
        let missing = || BaseRefusal::LocalDefaultMissing {
            branch: branch.to_owned(),
        };
        if !self.is_valid_branch_name(branch)? {
            return Err(missing().into());
        }
        let commit = self.local_branch_commit(branch)?.ok_or_else(missing)?;
        Ok(SettledBase {
            source: BaseSource::LocalDefault,
            branch: branch.to_owned(),
            remote: None,
            base_ref: branch.to_owned(),
            commit,
        })
    }

    fn settle_typed(
        &self,
        branch: &str,
        traced_to_user: bool,
        delivery: DeliveryMode,
    ) -> Result<SettledBase, GitError> {
        let branch = branch.trim();
        let branch = branch
            .strip_prefix("refs/heads/")
            .or_else(|| branch.strip_prefix("origin/"))
            .unwrap_or(branch);
        if !traced_to_user {
            return Err(BaseRefusal::NotTracedToUser {
                branch: branch.to_owned(),
            }
            .into());
        }
        if delivery != DeliveryMode::DirectPr {
            return Err(BaseRefusal::NeedsPullRequest {
                branch: branch.to_owned(),
                delivery,
            }
            .into());
        }
        if !self.is_valid_branch_name(branch)? {
            return Err(BaseRefusal::InvalidBranchName {
                branch: branch.to_owned(),
            }
            .into());
        }
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return Err(BaseRefusal::NoRemote {
                branch: branch.to_owned(),
            }
            .into());
        }
        if !remotes.iter().any(|r| r == ORIGIN) {
            return Err(BaseRefusal::NoOriginRemote { remotes }.into());
        }

        // Ask the remote first, so "no such branch" isn't confused with a network failure.
        let full = format!("refs/heads/{branch}");
        let fetch_failed = |detail: String| BaseRefusal::FetchFailed {
            remote: ORIGIN.to_owned(),
            branch: branch.to_owned(),
            detail,
        };
        let listed = self
            .network_command(
                ORIGIN,
                None,
                Credentials::Yours,
                ["ls-remote", "--exit-code", "--heads", ORIGIN, full.as_str()],
            )
            .and_then(Invocation::output)
            .map_err(|e| refusal_for_network_error(e, fetch_failed))?;
        match listed.code {
            Some(0)
                if listed
                    .stdout
                    .lines()
                    .any(|l| l.ends_with(&format!("\t{full}"))) => {}
            Some(0 | 2) => {
                return Err(BaseRefusal::NotOnRemote {
                    remote: ORIGIN.to_owned(),
                    branch: branch.to_owned(),
                }
                .into());
            }
            _ => return Err(fetch_failed(listed.stderr).into()),
        }

        let commit = self.fetch_branch(ORIGIN, branch)?;
        Ok(SettledBase {
            source: BaseSource::Typed,
            branch: branch.to_owned(),
            remote: Some(ORIGIN.to_owned()),
            base_ref: format!("{ORIGIN}/{branch}"),
            commit,
        })
    }

    /// Fetches one branch into its remote-tracking ref (`refs/remotes/<remote>/<branch>`) and
    /// returns the fetched commit; nothing else changes. A failure is a base-branch refusal
    /// ([`BaseRefusal::FetchFailed`]).
    pub(crate) fn fetch_branch(&self, remote: &str, branch: &str) -> Result<CommitSha, GitError> {
        let tracking = format!("refs/remotes/{remote}/{branch}");
        let refspec = format!("+refs/heads/{branch}:{tracking}");
        let fetch_failed = |detail: String| BaseRefusal::FetchFailed {
            remote: remote.to_owned(),
            branch: branch.to_owned(),
            detail,
        };
        let out: GitOutput = {
            let _guard = self.write_lock();
            self.network_command(
                remote,
                None,
                Credentials::Yours,
                [
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--no-recurse-submodules",
                    "--quiet",
                    remote,
                    refspec.as_str(),
                ],
            )
            .and_then(Invocation::output)
            .map_err(|e| refusal_for_network_error(e, fetch_failed))?
        };
        if !out.success() {
            return Err(fetch_failed(out.stderr).into());
        }
        self.resolve_commit(&tracking)?.ok_or_else(|| {
            unexpected(
                "fetch",
                &format!("{tracking} doesn't resolve after a successful fetch"),
            )
        })
    }

    /// Whether git accepts `branch` as a plain branch name (not an option, not a revision
    /// expression).
    pub(crate) fn is_valid_branch_name(&self, branch: &str) -> Result<bool, GitError> {
        if branch.is_empty() || branch.starts_with('-') {
            return Ok(false);
        }
        let full = format!("refs/heads/{branch}");
        let out = self
            .git()
            .invoke(self.root(), ["check-ref-format", full.as_str()])
            .output()?;
        Ok(out.success())
    }
}

/// A timeout talking to the remote is a refusal with that reason, and repository config the
/// app's transport can't override a refusal saying how to remove it; other failures (git
/// missing) stay errors of the app.
fn refusal_for_network_error(
    error: GitError,
    refusal: impl FnOnce(String) -> BaseRefusal,
) -> GitError {
    match error {
        GitError::RepoTransportConfig {
            checkout,
            remote,
            settings,
        } => BaseRefusal::RepoTransportConfig {
            checkout,
            remote,
            settings,
        }
        .into(),
        GitError::TimedOut { after, .. } => refusal(format!(
            "the remote didn't answer within {}s",
            after.as_secs()
        ))
        .into(),
        other => other,
    }
}
