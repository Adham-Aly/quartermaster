//! Where a new task's branch starts: the request handed to `qm_git::Repo::settle_base`.
//!
//! The first rule that applies:
//!
//! 1. **Inherited**: a stronger-model retry takes its task's recorded base, a review the reviewed
//!    task's ("recorded on the task, so retries, reviews and cleanup reuse it"). A typed branch
//!    that differs is refused.
//! 2. **The Quartermaster** with `base_branch` (the override was checked in phase A): typed and
//!    traced to you. Without one: the default.
//! 3. **A secondmate**: the routed request's branch, else its standing branch (both authorised on
//!    your turns when stored); a branch it typed that doesn't match is refused. Neither: the
//!    default.

use qm_git::BaseRequest;
use qm_policy::base_branch::check_unchanged;
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::error::Refusal;

/// Who is starting the task, and what they typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Starter<'a> {
    Quartermaster {
        authority: Authority,
        typed: Option<&'a str>,
    },
    Secondmate {
        request: Option<&'a str>,
        standing: Option<&'a str>,
        typed: Option<&'a str>,
    },
}

/// The base recorded on the task a retry or review inherits from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedBase {
    /// `tasks.base_branch`: the typed branch, or `None` for a default base.
    pub typed: Option<String>,
}

/// Strips what git would read as naming the same branch (`origin/x`, `refs/heads/x`).
fn bare(branch: &str) -> &str {
    branch
        .strip_prefix("refs/heads/")
        .or_else(|| branch.strip_prefix("origin/"))
        .unwrap_or(branch)
}

/// The settle request for a new task, or a refusal.
pub(crate) fn base_request(
    starter: Starter<'_>,
    inherited: Option<&RecordedBase>,
) -> Result<BaseRequest, Refusal> {
    let typed = match starter {
        Starter::Quartermaster { typed, .. } | Starter::Secondmate { typed, .. } => typed,
    };
    if let Some(recorded) = inherited {
        return match (&recorded.typed, typed) {
            (Some(branch), Some(t)) => {
                check_unchanged(branch, bare(t))?;
                Ok(typed_request(branch))
            }
            (Some(branch), None) => Ok(typed_request(branch)),
            (None, Some(t)) => Err(Refusal::new(format!(
                "the task this builds on started from the default branch, so this one does too, not {t}"
            ))),
            (None, None) => Ok(BaseRequest::Default),
        };
    }
    match starter {
        Starter::Quartermaster { authority, typed } => match typed {
            Some(branch) => {
                check_override(authority, OverrideKind::BaseBranch)?;
                Ok(typed_request(bare(branch)))
            }
            None => Ok(BaseRequest::Default),
        },
        Starter::Secondmate {
            request,
            standing,
            typed,
        } => {
            let filled = request.or(standing);
            if let Some(t) = typed
                && Some(bare(t)) != filled.map(bare)
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
            Ok(filled.map_or(BaseRequest::Default, |b| typed_request(bare(b))))
        }
    }
}

fn typed_request(branch: &str) -> BaseRequest {
    BaseRequest::Typed {
        branch: branch.to_owned(),
        traced_to_user: true,
    }
}
