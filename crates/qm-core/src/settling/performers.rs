//! Carrying out your answer on an approval card.
//!
//! `HeldService::answer` has already claimed the row before any of these run, and only your Approve
//! from the TUI reaches `perform`. Each performer reads the stored payload and uses only its
//! fields.

use super::in_place::merge_in_place;
use super::land_pr::land_pr;
use super::push::push_branch;
use super::{Fact, Landing, Origin, SettlingService};
use crate::delivery::DeliveryService;
use crate::delivery::github_tools::{record_write, write_words};
use crate::held::{HeldError, HeldOutcome, HeldPerformer, LoadedHeld, YouLine};
use qm_github::{GithubWrite, RepoRef};
use qm_store::held::{HeldPayload, MergePayload};
use qm_types::BoxFuture;
use qm_types::api::HeldAnswer;
use qm_types::ids::{CommitSha, TaskId};

/// `you.` event kinds the performers write through `HeldService`.
pub const YOU_APPROVED_MERGE: &str = "you.approved_merge";
pub const YOU_REJECTED_MERGE: &str = "you.rejected_merge";
pub const YOU_APPROVED_PUSH: &str = "you.approved_push";
pub const YOU_REJECTED_PUSH: &str = "you.rejected_push";
pub const YOU_APPROVED_GITHUB_WRITE: &str = "you.approved_github_write";

/// Performs held merges, pushes and in-place merges (kinds `merge` and `push`).
#[derive(Clone, Debug)]
pub struct MergePerformer {
    settling: SettlingService,
}

impl MergePerformer {
    pub fn new(settling: SettlingService) -> Self {
        Self { settling }
    }
}

/// What the card held: the task, the head, and the words for the notes.
fn describe(payload: &HeldPayload) -> Option<(TaskId, CommitSha, Kind)> {
    match payload {
        HeldPayload::Merge(MergePayload::PullRequest { task, head, .. }) => {
            Some((*task, head.clone(), Kind::Pr))
        }
        HeldPayload::Merge(MergePayload::InPlace {
            task,
            head,
            target_branch,
            ..
        }) => Some((*task, head.clone(), Kind::InPlace(target_branch.clone()))),
        HeldPayload::Push(p) => Some((p.task, p.head.clone(), Kind::Push(p.branch.clone()))),
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Pr,
    InPlace(String),
    Push(String),
}

impl HeldPerformer for MergePerformer {
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>> {
        Box::pin(async move {
            // Only an Approve performs anything.
            if !matches!(answer, HeldAnswer::Approve) {
                return Err(HeldError::Failed(
                    "only Approve carries out a merge".to_owned(),
                ));
            }
            let Some((task, head, kind)) = describe(&held.payload) else {
                return Err(HeldError::Failed(
                    "this card isn't a merge or a push".to_owned(),
                ));
            };
            let origin = Origin::Approval { held: held.id };
            let landing = match &held.payload {
                HeldPayload::Merge(m @ MergePayload::PullRequest { .. }) => {
                    land_pr(&self.settling, m, origin).await
                }
                HeldPayload::Merge(m @ MergePayload::InPlace { .. }) => {
                    merge_in_place(&self.settling, m, origin).await
                }
                HeldPayload::Push(p) => push_branch(&self.settling, p, origin).await,
                _ => {
                    return Err(HeldError::Failed(
                        "this card isn't a merge or a push".to_owned(),
                    ));
                }
            }
            .map_err(|e| HeldError::Failed(e.to_string()))?;
            let short = head.short();
            let (you_kind, you_summary) = match &kind {
                Kind::Pr => (
                    YOU_APPROVED_MERGE,
                    format!("you approved the merge of {task} at {short}"),
                ),
                Kind::InPlace(target) => (
                    YOU_APPROVED_MERGE,
                    format!("you approved landing {task} at {short} in {target}"),
                ),
                Kind::Push(_) => (
                    YOU_APPROVED_PUSH,
                    format!("you approved the push of {task} at {short}"),
                ),
            };
            let (note, superseded) = match landing {
                Landing::Landed(Fact::MergedPr { pr, base, .. }) => {
                    (format!("Merged PR #{pr} at {short} into {base}."), false)
                }
                Landing::Landed(Fact::InPlace { branch, target, .. }) => (
                    format!("Merged {branch} at {short} into {target} in the user's checkout."),
                    false,
                ),
                Landing::Landed(
                    Fact::Delivered { branch, .. } | Fact::DeliveredLocal { branch, .. },
                ) => (format!("Delivered {branch} at {short}."), false),
                Landing::Landed(Fact::DoneInFolder { folder }) => (
                    format!("Delivered {task}: its changes are in {folder}."),
                    false,
                ),
                Landing::Pushed { branch, .. } => (
                    format!(
                        "Pushed {branch} at {short}; it counts as delivered once GitHub's checks pass on it, and you're woken with settled then."
                    ),
                    false,
                ),
                Landing::Unknown { reason } => (
                    format!(
                        "The merge of {task} at {short} isn't confirmed: {reason}. {task} stays settling until GitHub settles it, and you're woken either way."
                    ),
                    false,
                ),
                Landing::Refused { reason, superseded } => (
                    format!("Nothing was merged or pushed: {reason}. {task} is back to reported."),
                    superseded,
                ),
            };
            Ok(HeldOutcome {
                note,
                you: Some(YouLine {
                    kind: you_kind,
                    summary: you_summary,
                }),
                superseded,
            })
        })
    }

    fn on_reject<'a>(
        &'a self,
        held: &'a LoadedHeld,
    ) -> BoxFuture<'a, Result<Option<HeldOutcome>, HeldError>> {
        Box::pin(async move {
            let Some((task, head, kind)) = describe(&held.payload) else {
                return Ok(None);
            };
            let short = head.short();
            let (what, you_kind, you_summary) = match &kind {
                Kind::Pr | Kind::InPlace(_) => (
                    format!("merging {task} at {short}"),
                    YOU_REJECTED_MERGE,
                    format!("you rejected the merge of {task} at {short}"),
                ),
                Kind::Push(branch) => (
                    format!("pushing {branch} at {short}"),
                    YOU_REJECTED_PUSH,
                    format!("you rejected the push of {task} at {short}"),
                ),
            };
            let _lock = self.settling.delivery().locks().lock(task).await;
            self.settling
                .refuse(
                    task,
                    &head,
                    "the user rejected it",
                    Origin::Approval { held: held.id },
                    None,
                )
                .await
                .map_err(HeldError::Store)?;
            Ok(Some(HeldOutcome {
                note: format!("The user rejected {what}; {task} is back to reported."),
                you: Some(YouLine {
                    kind: you_kind,
                    summary: you_summary,
                }),
                superseded: false,
            }))
        })
    }
}

/// Performs a held GitHub write exactly as stored (kind `github_write`).
#[derive(Clone, Debug)]
pub struct GithubWritePerformer {
    delivery: DeliveryService,
}

impl GithubWritePerformer {
    pub fn new(delivery: DeliveryService) -> Self {
        Self { delivery }
    }
}

impl HeldPerformer for GithubWritePerformer {
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>> {
        Box::pin(async move {
            if !matches!(answer, HeldAnswer::Approve) {
                return Err(HeldError::Failed(
                    "only Approve writes to GitHub".to_owned(),
                ));
            }
            let HeldPayload::GithubWrite(p) = &held.payload else {
                return Err(HeldError::Failed(
                    "this card isn't a GitHub write".to_owned(),
                ));
            };
            let Some(repo) = RepoRef::parse(&p.repo) else {
                return Err(HeldError::Failed(format!("{:?} isn't owner/name", p.repo)));
            };
            self.delivery
                .github()
                .write(
                    &repo,
                    &GithubWrite {
                        action: p.action,
                        number: p.number,
                        title: p.title.clone(),
                        body: p.body.clone(),
                        labels: p.labels.clone(),
                    },
                )
                .await
                .map_err(|e| HeldError::Failed(e.plain()))?;
            let words = write_words(p.action, &p.repo, p.number);
            if let Err(e) = record_write(&self.delivery, p.task, &words).await {
                tracing::warn!(task = %p.task, error = ?e, "couldn't record a GitHub write");
            }
            Ok(HeldOutcome {
                note: format!("Done: {words}."),
                you: Some(YouLine {
                    kind: YOU_APPROVED_GITHUB_WRITE,
                    summary: format!("you approved {}'s {words}", p.task),
                }),
                superseded: false,
            })
        })
    }

    fn on_reject<'a>(
        &'a self,
        held: &'a LoadedHeld,
    ) -> BoxFuture<'a, Result<Option<HeldOutcome>, HeldError>> {
        Box::pin(async move {
            let HeldPayload::GithubWrite(p) = &held.payload else {
                return Ok(None);
            };
            let words = write_words(p.action, &p.repo, p.number);
            Ok(Some(HeldOutcome::new(format!(
                "The user declined {}'s {words}; nothing was written.",
                p.task
            ))))
        })
    }
}
