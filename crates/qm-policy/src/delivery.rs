//! What settling means under each delivery mode.
//!
//! | Delivery mode | Settling means |
//! |---|---|
//! | Direct pull request | "Merging the PR on GitHub at exactly the approved head commit" (always a squash merge, its commit taking the PR's title and description) |
//! | Branch only | "Pushing the branch to the remote and marking it delivered. The app never merges it; you do. Closing keeps the branch." |
//! | Build in place | "Merging the branch into whatever branch your checkout has checked out, only if your working tree is clean, under the same autonomy gate as PRs." |
//! | Folder | "Marking the task delivered: its changes are already in the folder. Nothing is merged, pushed or undone." |
//!
//! "The workspace has no remote, so settling a branch-only task can't push. Instead it marks the
//! branch delivered and keeps it locally, where you merge or delete it. Closing keeps the branch
//! too." A folder task has no branch, so there's nothing to keep and nothing to approve.
//!
//! Autonomy: "autonomy off → held action". With autonomy off, a branch-only settle is held too,
//! since every push to a remote is an outward action awaiting approval. A folder settle is never
//! held: the work is already where you asked for it.
//!
//! The gates in [`crate::merge_gate`] run first; this module decides what settling does.

use qm_types::domain::{Autonomy, DeliveryMode, MergeMethod, TaskKind};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, TaskId};

/// The facts settling reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettleFacts<'a> {
    pub kind: TaskKind,
    pub delivery: DeliveryMode,
    pub autonomy: Autonomy,
    /// The project has a remote (the personal workspace doesn't).
    pub has_remote: bool,
    /// The head commit being settled. `None` for a folder task, which has no branch.
    pub head: Option<&'a CommitSha>,
    /// Build in place: whether your checkout's working tree is clean right now.
    pub checkout_clean: Option<bool>,
}

/// What settling does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettlePlan {
    /// Answer-only work: nothing lands; the answer is saved and the task closes.
    CloseAnswered,
    /// Merge the pull request at exactly `head`. `method` is always [`MergeMethod::Squash`]: the
    /// project's merge-method setting doesn't apply to pull requests.
    MergePr {
        head: CommitSha,
        method: MergeMethod,
    },
    /// Push the branch and mark it delivered. Never merged by the app.
    PushBranch { head: CommitSha },
    /// No remote: mark the branch delivered and keep it locally.
    MarkDeliveredLocal { head: CommitSha },
    /// Merge the branch into your checkout's current branch.
    MergeInPlace { head: CommitSha },
    /// Folder work: the changes are already in the folder. Nothing is merged, pushed or held.
    MarkDone,
    /// Autonomy is off: store a held action for your approval of the inner plan.
    Hold(Box<SettlePlan>),
}

impl SettlePlan {
    /// Whether the plan ever merges. Branch-only never does.
    pub fn merges(&self) -> bool {
        match self {
            Self::MergePr { .. } | Self::MergeInPlace { .. } => true,
            Self::Hold(inner) => inner.merges(),
            Self::CloseAnswered
            | Self::PushBranch { .. }
            | Self::MarkDeliveredLocal { .. }
            | Self::MarkDone => false,
        }
    }
}

/// "Build in place needs a clean tree": refused while your working tree has changes, at settling
/// and again when an approval is carried out.
pub fn check_checkout_clean(clean: Option<bool>) -> Result<(), Refusal> {
    match clean {
        Some(true) => Ok(()),
        Some(false) => Err(Refusal::new(
            "your checkout has uncommitted changes; build-in-place work merges only into a clean working tree",
        )),
        None => Err(Refusal::new(
            "your checkout's working tree couldn't be checked, so nothing was merged",
        )),
    }
}

/// What settling does under the project's delivery mode and autonomy. Run after
/// [`crate::merge_gate::check_mergeable`] for work that changes code.
pub fn plan(f: &SettleFacts<'_>) -> Result<SettlePlan, Refusal> {
    if f.kind.is_answer_only() {
        return Ok(SettlePlan::CloseAnswered);
    }
    if f.delivery == DeliveryMode::Folder {
        return Ok(SettlePlan::MarkDone);
    }
    let head = f
        .head
        .cloned()
        .ok_or_else(|| Refusal::new("the task has no branch head, so there's nothing to settle"))?;
    let action = match f.delivery {
        DeliveryMode::DirectPr => {
            if !f.has_remote {
                return Err(Refusal::new(
                    "the project has no remote, so a pull request can't be merged",
                ));
            }
            SettlePlan::MergePr {
                head,
                method: MergeMethod::Squash,
            }
        }
        DeliveryMode::BranchOnly if f.has_remote => SettlePlan::PushBranch { head },
        // Nothing leaves the Mac, so there's nothing to approve.
        DeliveryMode::BranchOnly => return Ok(SettlePlan::MarkDeliveredLocal { head }),
        DeliveryMode::BuildInPlace => {
            check_checkout_clean(f.checkout_clean)?;
            SettlePlan::MergeInPlace { head }
        }
        DeliveryMode::Folder => return Ok(SettlePlan::MarkDone),
    };
    Ok(match f.autonomy {
        Autonomy::Off => SettlePlan::Hold(Box::new(action)),
        Autonomy::On => action,
    })
}

/// "Closing keeps the branch" for branch-only work (and the workspace's local branches); other
/// modes save unmerged work to the kept ref, then remove the branch. A folder task has no branch
/// and no kept ref: its files stay where they are.
pub fn close_keeps_branch(delivery: DeliveryMode) -> bool {
    delivery == DeliveryMode::BranchOnly
}

/// Folder work writes straight into one folder with no git to undo a clobbered file, so only one
/// code-writing task may work in it at a time. Answer-only tasks run alongside. `writer` is the open
/// code task already in the folder, if any.
pub fn check_folder_writer(
    delivery: DeliveryMode,
    kind: TaskKind,
    writer: Option<TaskId>,
) -> Result<(), Refusal> {
    match writer {
        Some(other) if delivery == DeliveryMode::Folder && kind.can_change_code() => {
            Err(Refusal::new(format!(
                "the personal folder already has {other} working in it; wait for it to finish, then start this task"
            )))
        }
        _ => Ok(()),
    }
}
