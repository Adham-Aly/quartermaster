//! Retiring a task's worktree.
//!
//! When a task closes, by any exit (landed, failed, stopped, discarded), its worktree is removed
//! (uncommitted changes with it) and so is the branch `qm/task-0042`, with its compare-and-swap on
//! the tip read here. Unmerged commits are no longer saved anywhere: once the branch is gone they
//! are unreachable and git garbage-collects them as it does any other unreachable object. The one
//! exception is a delivered branch-only task, whose branch is the deliverable and is kept
//! ([`RetireSpec::keep_branch`]).
//!
//! **Crash-safety.** Every step is idempotent: the worktree goes first, then the branch with a
//! compare-and-swap on the tip it was read at. An interrupted retirement is finished by calling
//! [`Repo::retire`] again.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use qm_types::data_dir::TaskPaths;
use qm_types::ids::{CommitSha, TaskId};

use crate::error::GitError;
use crate::repo::{Repo, WorktreeEntry, normalize_path};

/// Everything needed to retire a task's worktree.
#[derive(Clone, Copy, Debug)]
pub struct RetireSpec<'a> {
    /// The task; names the worktree folder and the branch.
    pub task: TaskId,
    /// The task's folders; the worktree is at `paths.worktree`.
    pub paths: &'a TaskPaths,
    /// Leave the task's branch in place after removing the worktree: delivered branch-only work,
    /// whose branch is the deliverable (closing keeps the branch). Other exits delete it.
    pub keep_branch: bool,
}

impl<'a> RetireSpec<'a> {
    /// A retirement of `task` with the branch deleted; set [`RetireSpec::keep_branch`] to keep it.
    pub fn new(task: TaskId, paths: &'a TaskPaths) -> Self {
        Self {
            task,
            paths,
            keep_branch: false,
        }
    }

    /// Sets [`RetireSpec::keep_branch`].
    #[must_use]
    pub fn keep_branch(mut self, keep_branch: bool) -> Self {
        self.keep_branch = keep_branch;
        self
    }
}

/// What retiring did. Everything in it is also true after a repeated call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retirement {
    /// What happened to the worktree.
    pub worktree: WorktreeRemoval,
    /// What happened to the branch.
    pub branch: BranchRemoval,
}

/// What happened to the task's worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeRemoval {
    /// It was removed now.
    Removed { path: PathBuf },
    /// It wasn't there (never created, or removed by an earlier attempt).
    AlreadyGone,
}

/// What happened to the task's branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchRemoval {
    /// It was deleted now; it pointed at `commit`.
    Deleted { commit: CommitSha },
    /// It wasn't there.
    AlreadyGone,
    /// It is checked out in another worktree (perhaps your checkout), so it was left alone.
    CheckedOutElsewhere { path: PathBuf },
    /// It was left in place because the spec asked to keep it ([`RetireSpec::keep_branch`]); it
    /// points at `commit`.
    Kept { commit: CommitSha },
}

impl Repo {
    /// Retires a task's worktree: removes it, then deletes the branch. Safe to call again after an
    /// interruption, and on a task whose worktree was never created.
    ///
    /// The core stops the task's crewmate (and its background jobs) first, so nothing writes to
    /// the worktree while it goes. A commit made after the branch was read makes the branch
    /// deletion's compare-and-swap fail (an error; calling again picks it up).
    pub fn retire(&self, spec: &RetireSpec<'_>) -> Result<Retirement, GitError> {
        let _guard = self.write_lock();
        let task = spec.task;
        let path = normalize_path(&spec.paths.worktree);
        let worktrees = self.worktrees()?;
        let ours = worktrees.iter().find(|w| w.path == path && !w.is_main);
        let present = worktree_present(task, &path, ours.is_some())?;
        let branch_tip = self.resolve_commit(&format!("refs/heads/{}", task.branch()))?;

        let worktree = self.remove_task_worktree(ours, &path, present)?;
        let branch = if spec.keep_branch {
            branch_tip.map_or(BranchRemoval::AlreadyGone, |commit| BranchRemoval::Kept {
                commit,
            })
        } else {
            self.delete_task_branch(task, branch_tip)?
        };
        Ok(Retirement { worktree, branch })
    }

    /// Removes the task's worktree (and any empty leftover folder at its path).
    fn remove_task_worktree(
        &self,
        ours: Option<&WorktreeEntry>,
        path: &Path,
        present: bool,
    ) -> Result<WorktreeRemoval, GitError> {
        if let Some(entry) = ours {
            self.remove_worktree(entry, present)?;
        }
        if !present && path.is_dir() {
            // An empty leftover folder (checked by `worktree_present`).
            fs::remove_dir(path).map_err(|e| GitError::io("remove", path, e))?;
        }
        Ok(if present {
            WorktreeRemoval::Removed {
                path: path.to_owned(),
            }
        } else {
            WorktreeRemoval::AlreadyGone
        })
    }

    /// Deletes the task's branch at exactly `tip` (compare-and-swap), unless it's checked out
    /// in another worktree.
    fn delete_task_branch(
        &self,
        task: TaskId,
        tip: Option<CommitSha>,
    ) -> Result<BranchRemoval, GitError> {
        let Some(tip) = tip else {
            return Ok(BranchRemoval::AlreadyGone);
        };
        let branch = task.branch();
        let elsewhere = self
            .worktrees()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch.as_str()) && w.prunable.is_none());
        if let Some(other) = elsewhere {
            return Ok(BranchRemoval::CheckedOutElsewhere { path: other.path });
        }
        let branch_ref = format!("refs/heads/{branch}");
        self.run(["update-ref", "-d", branch_ref.as_str(), tip.as_str()])?;
        Ok(BranchRemoval::Deleted { commit: tip })
    }

    /// Removes our worktree: through git when its folder is there, else only git's record of it.
    fn remove_worktree(&self, entry: &WorktreeEntry, present: bool) -> Result<(), GitError> {
        if present {
            let path = entry.path.as_os_str();
            let out = self
                .git()
                .invoke(
                    self.root(),
                    [
                        OsStr::new("worktree"),
                        OsStr::new("remove"),
                        OsStr::new("--force"),
                        path,
                    ],
                )
                .output()?;
            if out.success() {
                return Ok(());
            }
            if entry.locked.is_some() {
                // Someone locked it on purpose; leave it for them.
                return Err(GitError::Failed {
                    command: "worktree remove".to_owned(),
                    code: out.code,
                    stderr: out.stderr,
                });
            }
            // Git refuses some worktrees (for example with submodules checked out). The folder is
            // the task's own, so remove it directly.
            tracing::warn!(path = %entry.path.display(), stderr = %out.stderr, "git couldn't remove the worktree; removing its folder");
            fs::remove_dir_all(&entry.path)
                .map_err(|e| GitError::io("remove the worktree", &entry.path, e))?;
        }
        self.forget_worktree(&entry.path)
    }

    /// Removes git's administrative record of the worktree at `path` (whose folder is gone),
    /// and only that one: unlike `git worktree prune`, other missing worktrees (say, on an
    /// unplugged disk) are left alone.
    fn forget_worktree(&self, path: &Path) -> Result<(), GitError> {
        let admin_root = self.common_dir().join("worktrees");
        let entries = match fs::read_dir(&admin_root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(GitError::io("read", admin_root, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| GitError::io("read", &admin_root, e))?;
            let Ok(gitdir) = fs::read_to_string(entry.path().join("gitdir")) else {
                continue;
            };
            let recorded = normalize_path(Path::new(gitdir.trim()));
            if recorded.parent() == Some(path) || recorded == path.join(".git") {
                let admin = entry.path();
                fs::remove_dir_all(&admin)
                    .map_err(|e| GitError::io("remove git's record of", admin, e))?;
            }
        }
        Ok(())
    }
}

/// Whether the task's worktree is there with work that could be in it. An absent path or
/// an empty leftover folder is not; anything else at the path that isn't a registered git
/// worktree is an error, and is left untouched.
fn worktree_present(task: TaskId, path: &Path, registered: bool) -> Result<bool, GitError> {
    if registered && path.join(".git").is_file() {
        return Ok(true);
    }
    if is_absent_or_empty(path)? {
        return Ok(false);
    }
    Err(GitError::WorktreePathOccupied {
        path: path.to_owned(),
        branch: task.branch(),
    })
}

fn is_absent_or_empty(path: &Path) -> Result<bool, GitError> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(GitError::io("read", path, e)),
        Ok(meta) if meta.is_dir() => Ok(fs::read_dir(path)
            .map_err(|e| GitError::io("read", path, e))?
            .next()
            .is_none()),
        Ok(_) => Ok(false),
    }
}
