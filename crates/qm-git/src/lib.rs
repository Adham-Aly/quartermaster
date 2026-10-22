//! Worktrees and refs, through your installed `git`.
//!
//! Every change the app makes runs the real `git` behind a typed Rust wrapper. The app's own
//! commands strip inherited `GIT_*` variables and skip hooks; a crewmate's commits run the
//! project's hooks as usual. This crate creates a task's worktree at `tasks/T-0042/worktree` on
//! branch `qm/task-0042` (names from `qm_types::ids::TaskId`), resolves base branches (the
//! remote's default branch freshly fetched, or a typed base branch that traces back to you),
//! removes a retired task's worktree and branch (unmerged commits are left for git to collect),
//! and lands build-in-place work into your checkout's current branch only when its working tree
//! is clean.
//!

//! # The pieces
//!
//! - [`Git`] runs your `git` the way the app's own commands must run.
//! - [`Repo`] is a project's repository, opened from your checkout.
//! - [`Repo::settle_base`] decides where a task starts, from a [`BaseRequest`] and facts the core
//!   supplies (whether a typed branch traces to you, the delivery mode), giving a [`SettledBase`]
//!   or a [`BaseRefusal`] with the reason.
//! - [`Repo::prepare_worktree`] creates the worktree, copies the project's listed files and runs
//!   its setup command; [`run_setup`] runs a setup command on its own.
//! - [`Repo::retire`] removes the worktree, then the branch, idempotently.
//! - [`Repo::reattach_worktree`] adds a vanished worktree back on its existing branch.
//! - [`Repo::worktree_head`], [`Repo::own_commits`] and [`Repo::changed_files`] read a task's
//!   head, commit messages and changed files for its report and reviews.
//! - [`reconcile`] matches worktrees on disk against tasks at startup and deletes nothing.
//!
//! Everything is synchronous: the core runs these calls on a blocking thread
//! (`tokio::task::spawn_blocking`). Landing build-in-place work starts from [`Repo::is_clean`].

mod base;
mod error;
mod git;
mod inspect;
mod land;
mod lock;
mod network;
mod process;
mod reconcile;
mod repo;
mod retire;
mod setup;
mod worktree;

pub use base::{BaseRequest, BaseSource, SettledBase};
pub use error::{BaseRefusal, GitError};
pub use git::Git;
pub use inspect::OwnCommits;
pub use land::{DiffStat, InPlaceMerge, InPlaceOutcome, PushCredential, PushOutcome, PushSpec};
pub use network::RepoTransportSetting;
pub use reconcile::{
    KeptRef, MissingWorktree, OrphanBranch, ReconcileProject, ReconcileReport, StaleRegistration,
    UnexpectedReason, UnexpectedWorktree, UnreadableProject, reconcile,
};
pub use repo::{ORIGIN, Repo, WorktreeEntry};
pub use retire::{BranchRemoval, RetireSpec, Retirement, WorktreeRemoval};
pub use setup::{DEFAULT_SETUP_TIMEOUT, SetupCommand, SetupOutcome, run_setup};
pub use worktree::{CopyReport, PreparedWorktree, ReattachedWorktree, WorktreeSpec};
