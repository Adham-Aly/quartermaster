//! What settling means under each delivery mode.
//!
//! | Delivery mode | Settling means |
//! |---|---|
//! | Direct pull request | "Merging the PR on GitHub at exactly the approved head commit" (always a squash merge, its commit taking the PR's title and description) |
//! | Branch only | "Pushing the branch to the remote and marking it delivered. The app never merges it; you do. Closing keeps the branch." |
//! | Build in place | "Merging the branch into whatever branch your checkout has checked out, only if your working tree is clean, under the same autonomy gate as PRs." |
//! | Folder | "Marking the task delivered: its changes are already in the folder. Nothing is merged, pushed or undone." |
//!
//! "A repository with no remote can't push, so settling a branch-only task there can't either. Instead it marks the
//! branch delivered and keeps it locally, where you merge or delete it. Closing keeps the branch
//! too." A folder task has no branch, so there's nothing to keep and nothing to approve.
//!
//! Autonomy: "autonomy normal → held action". Under normal autonomy, a branch-only settle is held too,
//! since every push to a remote is an outward action awaiting approval. A folder settle is never
//! held: the work is already where you asked for it.
//!
//! The gates in [`crate::merge_gate`] run first; this module decides what settling does.
//!
//! What a task delivered, code or an answer, is decided by what its crewmate did
//! ([`deliverable`]), never by its kind: the kind routes and guides, it doesn't forbid.

use qm_types::domain::{Autonomy, DeliveryMode, MergeMethod, TaskKind};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, TaskId};
use std::path::Path;

/// What a task delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deliverable {
    /// Commits on its branch, or edits in place reported with the headings.
    Code,
    /// An answer: nothing lands, and the task closes as answered.
    Answer,
}

/// The facts [`deliverable`] reads, from the task's newest clear done report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeliveredFacts {
    /// The task works in place (folder delivery): no branch, its edits are final.
    pub in_place: bool,
    /// A worktree task reported a head with commits of its own on top of its base.
    pub own_commits: bool,
    /// The report carries the code headings (Intent, What changed, …).
    pub headings: bool,
}

/// Code or answer, by what the crewmate did: a worktree task delivered code when it reported
/// commits of its own; an in-place task when its done report carries the headings. Any kind may
/// deliver either.
pub fn deliverable(f: DeliveredFacts) -> Deliverable {
    let code = if f.in_place {
        f.headings
    } else {
        f.own_commits
    };
    if code {
        Deliverable::Code
    } else {
        Deliverable::Answer
    }
}

/// The facts settling reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettleFacts<'a> {
    pub deliverable: Deliverable,
    pub delivery: DeliveryMode,
    pub autonomy: Autonomy,
    /// The project has a remote (plain folders and local-only repositories don't).
    pub has_remote: bool,
    /// The head commit being settled. `None` for a folder task, which has no branch.
    pub head: Option<&'a CommitSha>,
    /// Build in place: whether your checkout's working tree is clean right now.
    pub checkout_clean: Option<bool>,
}

/// What settling does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettlePlan {
    /// An answer: nothing lands; the answer is saved and the task closes.
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
    if f.deliverable == Deliverable::Answer {
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
        Autonomy::Normal => SettlePlan::Hold(Box::new(action)),
        Autonomy::Auto => action,
    })
}

/// "Closing keeps the branch" for branch-only work (and the workspace's local branches); other
/// modes remove the branch. A folder task has no branch: its files stay where they are.
pub fn close_keeps_branch(delivery: DeliveryMode) -> bool {
    delivery == DeliveryMode::BranchOnly
}

/// Whether two folders overlap: the same folder, or one inside the other. Paths are compared as
/// given, component by component (the caller canonicalises them).
pub fn folders_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// In-place work writes straight into a folder with no branch to undo a clobbered file, so only
/// one code-kind task (build, fix, unclear) may work in place in overlapping folders at a time.
/// Other kinds run alongside, and worktree tasks never conflict. `open` is every open in-place
/// task: its ID, folder and kind.
pub fn check_folder_writer(
    in_place: bool,
    folder: &Path,
    kind: TaskKind,
    open: &[(TaskId, &Path, TaskKind)],
) -> Result<(), Refusal> {
    if !in_place || !kind.can_change_code() {
        return Ok(());
    }
    match open
        .iter()
        .find(|(_, other, k)| k.can_change_code() && folders_overlap(folder, other))
    {
        Some((other, _, _)) => Err(Refusal::new(format!(
            "{} already has {other} working in place; wait for it, or use a worktree",
            folder.display()
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn deliverable_follows_what_the_crewmate_did() {
        let f = |in_place, own_commits, headings| {
            deliverable(DeliveredFacts {
                in_place,
                own_commits,
                headings,
            })
        };
        assert_eq!(f(false, true, false), Deliverable::Code);
        assert_eq!(f(false, true, true), Deliverable::Code);
        assert_eq!(f(false, false, true), Deliverable::Answer, "no commits");
        assert_eq!(f(true, false, true), Deliverable::Code);
        assert_eq!(f(true, false, false), Deliverable::Answer);
        assert_eq!(
            f(true, true, false),
            Deliverable::Answer,
            "in place has no branch"
        );
    }

    fn facts(deliverable: Deliverable, delivery: DeliveryMode, autonomy: Autonomy) -> SettlePlan {
        let head: CommitSha = "a".repeat(40).parse().expect("sha");
        plan(&SettleFacts {
            deliverable,
            delivery,
            autonomy,
            has_remote: true,
            head: Some(&head),
            checkout_clean: Some(true),
        })
        .expect("a plan")
    }

    #[test]
    fn an_answer_closes_under_every_delivery_mode_and_code_follows_the_mode() {
        for mode in [
            DeliveryMode::DirectPr,
            DeliveryMode::BranchOnly,
            DeliveryMode::BuildInPlace,
            DeliveryMode::Folder,
        ] {
            assert_eq!(
                facts(Deliverable::Answer, mode, Autonomy::Auto),
                SettlePlan::CloseAnswered
            );
        }
        assert!(matches!(
            facts(Deliverable::Code, DeliveryMode::DirectPr, Autonomy::Auto),
            SettlePlan::MergePr { .. }
        ));
        assert!(matches!(
            facts(Deliverable::Code, DeliveryMode::DirectPr, Autonomy::Normal),
            SettlePlan::Hold(_)
        ));
        assert_eq!(
            facts(Deliverable::Code, DeliveryMode::Folder, Autonomy::Normal),
            SettlePlan::MarkDone
        );
    }

    #[test]
    fn one_code_writer_per_overlapping_folder() {
        let t = |n: u32| -> TaskId { format!("T-{n:04}").parse().expect("id") };
        let site = PathBuf::from("/Users/u/site");
        let docs = PathBuf::from("/Users/u/site/docs");
        let other = PathBuf::from("/Users/u/other");
        let open = [(t(1), site.as_path(), TaskKind::Build)];
        // Same folder, inside it, and above it are refused for code kinds working in place.
        assert!(check_folder_writer(true, &site, TaskKind::Fix, &open).is_err());
        assert!(check_folder_writer(true, &docs, TaskKind::Build, &open).is_err());
        let open_docs = [(t(2), docs.as_path(), TaskKind::Unclear)];
        let refusal = check_folder_writer(true, &site, TaskKind::Build, &open_docs)
            .expect_err("above an open writer");
        assert!(refusal.to_string().contains("T-0002"), "{refusal}");
        // A sibling folder, an answer kind, or a worktree task never conflicts.
        assert!(check_folder_writer(true, &other, TaskKind::Build, &open).is_ok());
        assert!(check_folder_writer(true, &site, TaskKind::Lookup, &open).is_ok());
        assert!(check_folder_writer(false, &site, TaskKind::Build, &open).is_ok());
        let reader = [(t(3), site.as_path(), TaskKind::Research)];
        assert!(check_folder_writer(true, &site, TaskKind::Build, &reader).is_ok());
        // Siblings that share a prefix of characters, not of components, don't overlap.
        assert!(!folders_overlap(
            Path::new("/Users/u/site"),
            Path::new("/Users/u/site-2")
        ));
    }
}
