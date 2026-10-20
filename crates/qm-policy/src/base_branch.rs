//! Where a task's branch starts.
//!
//! "Default (enforced). With no branch named anywhere, a task fetches and starts from
//! `origin/<default branch>`. If fetching or finding the default branch fails, the task is refused
//! with the reason, rather than built on stale code. A project with no remote starts from its local
//! default branch. No agent can change this default; only a branch that traces back to you can."
//!
//! "A typed `base_branch`. The task starts from `origin/<base_branch>` and its pull request
//! targets that same branch. The branch is recorded on the task, so retries, reviews and cleanup
//! reuse it, and it can't change after the task starts. It's refused when the project has no
//! remote, the branch doesn't exist on the remote, or the delivery mode isn't a pull request."
//!
//! | Who supplies it | When the app accepts it |
//! |---|---|
//! | The Quartermaster | Only on a turn started by your own message, never on one started by an app event. |
//! | A request routed to a secondmate | The request carries a typed `base_branch`, accepted under the same rule and stored on the request. |
//! | A secondmate's standing branch | A typed field on the secondmate's record … set or changed only on a turn started by your message. |
//! | A secondmate starting a crewmate | The app fills it in: from the routed request if it has one, otherwise the standing branch, otherwise the default. The secondmate can't pass any other branch, and the app refuses one that doesn't match. |

use crate::names::check_branch_name;
use crate::overrides::{Authority, OverrideKind, check_override};
use qm_types::domain::DeliveryMode;
use qm_types::error::Refusal;

/// Who is starting the task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Starter<'a> {
    /// The Quartermaster, with a typed `base_branch` if it passed one.
    Quartermaster {
        authority: Authority,
        typed: Option<&'a str>,
    },
    /// A secondmate. `request` and `standing` were accepted on your turns when stored. `typed`
    /// is what the secondmate passed, if anything: it must match the filled-in branch.
    Secondmate {
        request: Option<&'a str>,
        standing: Option<&'a str>,
        typed: Option<&'a str>,
    },
}

/// The project's side of the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectGit<'a> {
    pub delivery: DeliveryMode,
    pub has_remote: bool,
    /// The remote's default branch, freshly fetched; `Err` carries why fetching or finding it
    /// failed. Ignored for a project with no remote.
    pub remote_default: Result<&'a str, &'a str>,
    /// The local default branch, for a project with no remote.
    pub local_default: &'a str,
    /// The branches on the remote, after fetching.
    pub remote_branches: &'a [&'a str],
}

/// Where the task starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// `origin/<default>`, freshly fetched.
    RemoteDefault { branch: String },
    /// A project with no remote: its local default branch.
    LocalDefault { branch: String },
    /// A typed base branch that traces back to you: start from `origin/<branch>`, and the pull
    /// request targets it.
    Typed { branch: String },
}

impl Base {
    /// The branch, as recorded on the task.
    pub fn branch(&self) -> &str {
        match self {
            Self::RemoteDefault { branch }
            | Self::LocalDefault { branch }
            | Self::Typed { branch } => branch,
        }
    }
}

fn accept_typed(branch: &str, project: &ProjectGit<'_>) -> Result<Base, Refusal> {
    check_branch_name(branch)?;
    if !project.has_remote {
        return Err(Refusal::new(format!(
            "base branch {branch} can't be used: the project has no remote"
        )));
    }
    if project.delivery != DeliveryMode::DirectPr {
        return Err(Refusal::new(format!(
            "base branch {branch} can't be used: the project doesn't deliver by pull request"
        )));
    }
    if !project.remote_branches.contains(&branch) {
        return Err(Refusal::new(format!(
            "base branch {branch} doesn't exist on the remote"
        )));
    }
    Ok(Base::Typed {
        branch: branch.to_owned(),
    })
}

fn default_base(project: &ProjectGit<'_>) -> Result<Base, Refusal> {
    if !project.has_remote {
        return Ok(Base::LocalDefault {
            branch: project.local_default.to_owned(),
        });
    }
    match project.remote_default {
        Ok(branch) => Ok(Base::RemoteDefault {
            branch: branch.to_owned(),
        }),
        Err(why) => Err(Refusal::new(format!(
            "the task can't start: fetching the remote's default branch failed ({why})"
        ))),
    }
}

/// Settles the base for a new task, or refuses with the reason.
pub fn resolve(starter: Starter<'_>, project: &ProjectGit<'_>) -> Result<Base, Refusal> {
    match starter {
        Starter::Quartermaster { authority, typed } => match typed {
            Some(branch) => {
                check_override(authority, OverrideKind::BaseBranch)?;
                accept_typed(branch, project)
            }
            None => default_base(project),
        },
        Starter::Secondmate {
            request,
            standing,
            typed,
        } => {
            let filled = request.or(standing);
            if let Some(t) = typed
                && Some(t) != filled
            {
                return Err(Refusal::new(match filled {
                    Some(f) => format!(
                        "a secondmate can't choose a base branch; this task starts from {f}, not {t}"
                    ),
                    None => format!(
                        "a secondmate can't choose a base branch; this task starts from the default branch, not {t}"
                    ),
                }));
            }
            match filled {
                Some(branch) => accept_typed(branch, project),
                None => default_base(project),
            }
        }
    }
}

/// "It can't change after the task starts."
pub fn check_unchanged(recorded: &str, requested: &str) -> Result<(), Refusal> {
    if recorded == requested {
        Ok(())
    } else {
        Err(Refusal::new(format!(
            "the task's base branch is {recorded} and can't change after it starts"
        )))
    }
}

/// A secondmate's standing branch: set or changed only on your turn, and a valid branch name.
/// Affects only crewmates started afterwards.
pub fn check_standing_branch(authority: Authority, branch: Option<&str>) -> Result<(), Refusal> {
    check_override(authority, OverrideKind::StandingBranch)?;
    branch.map_or(Ok(()), check_branch_name)
}
