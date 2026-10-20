//! Retiring a task's worktree, keeping its work.
//!
//! When a task closes, by any exit (landed, failed, stopped, discarded), its worktree is retired
//! the same way: unmerged work is saved to `refs/quartermaster/kept/T-0042`, then the worktree and
//! the branch `qm/task-0042` are removed.
//!
//! "Work" is everything the crewmate could have left behind, gathered before anything is removed:
//!
//! - the worktree's uncommitted changes, including new untracked files (ignored files are not
//!   kept), saved as a snapshot commit on top of its `HEAD` without touching the worktree, its
//!   index or any branch;
//! - the worktree's `HEAD`, even if the crewmate switched away from its branch;
//! - the tip of the task's branch;
//! - whatever an earlier, interrupted retirement already saved to the kept ref.
//!
//! Anything already contained in the base (or in a ref the core says the work landed in) is not
//! "unmerged" and needs no keeping. The rest goes to the kept ref; when there are several
//! unrelated heads, a keep commit with each as a parent holds them all. The kept ref only ever
//! moves forward to a commit containing its previous value; this module never deletes one.
//!
//! **Crash-safety.** Every step is idempotent and ordered so that nothing is removed until its
//! work is reachable from the kept ref (or the base): save, verify, then remove the worktree,
//! then delete the branch with a compare-and-swap on the exact commit verified. An interrupted
//! retirement is finished by calling [`Repo::retire`] again.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use qm_types::data_dir::TaskPaths;
use qm_types::ids::{CommitSha, TaskId};

use crate::error::GitError;
use crate::repo::{Repo, WorktreeEntry, normalize_path, parse_sha};

/// Name and address on the app's own snapshot and keep commits.
const APP_IDENTITY: (&str, &str) = ("Quartermaster", "quartermaster@localhost");

/// The temporary index used to snapshot uncommitted work, inside the task's folder (outside the
/// worktree).
const SNAPSHOT_INDEX: &str = ".kept-index";

/// Everything needed to retire a task's worktree.
#[derive(Clone, Copy, Debug)]
pub struct RetireSpec<'a> {
    /// The task; names the branch and the kept ref.
    pub task: TaskId,
    /// The task's folders; the worktree is at `paths.worktree`.
    pub paths: &'a TaskPaths,
    /// The commit the task started at (`tasks.base_commit`), if recorded: work it contains isn't
    /// the task's.
    pub base_commit: Option<&'a CommitSha>,
    /// Revisions whose history counts as merged: the task's base ref (`origin/main`) and, for
    /// work that landed, where it landed. Ones that don't resolve are ignored. A squash merge
    /// leaves no ancestry, so its work is kept too, which is harmless.
    pub merged_into: &'a [&'a str],
    /// Leave the task's branch in place after saving the kept ref and removing the worktree:
    /// delivered branch-only work, whose branch is the deliverable (closing keeps the branch).
    /// Side exits pass `false` and the branch is deleted as usual.
    pub keep_branch: bool,
}

impl<'a> RetireSpec<'a> {
    /// A retirement of `task` with nothing counted as merged and the branch deleted; set the
    /// other fields with the methods below or directly.
    pub fn new(task: TaskId, paths: &'a TaskPaths) -> Self {
        Self {
            task,
            paths,
            base_commit: None,
            merged_into: &[],
            keep_branch: false,
        }
    }

    /// Sets [`RetireSpec::base_commit`].
    #[must_use]
    pub fn base_commit(mut self, base_commit: Option<&'a CommitSha>) -> Self {
        self.base_commit = base_commit;
        self
    }

    /// Sets [`RetireSpec::merged_into`].
    #[must_use]
    pub fn merged_into(mut self, merged_into: &'a [&'a str]) -> Self {
        self.merged_into = merged_into;
        self
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
    /// The kept ref, if the task left unmerged work (now or in an earlier attempt).
    pub kept: Option<KeptWork>,
    /// What happened to the worktree.
    pub worktree: WorktreeRemoval,
    /// What happened to the branch.
    pub branch: BranchRemoval,
}

/// Unmerged work saved under `refs/quartermaster/kept/T-0042`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptWork {
    /// The full ref name.
    pub git_ref: String,
    /// The commit it points at.
    pub commit: CommitSha,
    /// Whether this retirement saved uncommitted changes from the worktree into it.
    pub includes_uncommitted: bool,
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
    /// It was deleted now; it pointed at `commit`, which the kept ref or the base contains.
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
    /// Retires a task's worktree: saves unmerged work to its kept ref, then removes the worktree
    /// and the branch. Safe to call again after an interruption, and on a task whose worktree was
    /// never created.
    ///
    /// Nothing is removed unless saving succeeded: if the worktree is there but its state can't be
    /// read, this fails and leaves everything in place.
    ///
    /// The core stops the task's crewmate (and its background jobs) first: a change written after
    /// the snapshot would go with the worktree, and a commit made after the branch was read makes
    /// the branch deletion's compare-and-swap fail (an error; calling again picks it up).
    pub fn retire(&self, spec: &RetireSpec<'_>) -> Result<Retirement, GitError> {
        let _guard = self.write_lock();
        let task = spec.task;
        let path = normalize_path(&spec.paths.worktree);
        let worktrees = self.worktrees()?;
        let ours = worktrees.iter().find(|w| w.path == path && !w.is_main);
        let present = worktree_present(task, &path, ours.is_some())?;

        // 1. Gather the work (including a snapshot of uncommitted changes).
        let work = self.gather_work(spec, &path, present)?;
        // 2. Save what isn't merged to the kept ref.
        let merged = self.merged_commits(spec)?;
        let kept_commit = self.save_kept(task, &work, &merged)?;
        // 3. Verify every piece of work is reachable before removing anything.
        self.verify_reachable(task, &work, merged, kept_commit.as_ref())?;
        // 4. Remove the worktree, then 5. the branch (unless it's kept).
        let worktree = self.remove_task_worktree(ours, &path, present)?;
        let branch = if spec.keep_branch {
            work.branch_tip
                .clone()
                .map_or(BranchRemoval::AlreadyGone, |commit| BranchRemoval::Kept {
                    commit,
                })
        } else {
            self.delete_task_branch(task, work.branch_tip.clone())?
        };

        let kept = kept_commit.map(|commit| KeptWork {
            git_ref: task.kept_ref(),
            // A snapshot is a new commit, so it is never merged and always among the heads.
            includes_uncommitted: work.snapshot.is_some(),
            commit,
        });
        Ok(Retirement {
            kept,
            worktree,
            branch,
        })
    }

    /// Reads every piece of work the task could have left behind.
    fn gather_work(
        &self,
        spec: &RetireSpec<'_>,
        path: &Path,
        present: bool,
    ) -> Result<TaskWork, GitError> {
        let (worktree_head, snapshot) = if present {
            let head = self.worktree_head(path)?;
            let snapshot = match &head {
                Some(head) => self.snapshot_uncommitted(spec.task, spec.paths, path, head)?,
                None => None,
            };
            (head, snapshot)
        } else {
            (None, None)
        };
        Ok(TaskWork {
            worktree_head,
            snapshot,
            branch_tip: self.resolve_commit(&format!("refs/heads/{}", spec.task.branch()))?,
            existing_kept: self.resolve_commit(&spec.task.kept_ref())?,
        })
    }

    /// Points the kept ref at a commit containing all unmerged work (and whatever it held
    /// before). Returns the kept ref's commit afterwards, if it exists.
    fn save_kept(
        &self,
        task: TaskId,
        work: &TaskWork,
        merged: &[CommitSha],
    ) -> Result<Option<CommitSha>, GitError> {
        let mut heads: Vec<CommitSha> = Vec::new();
        for candidate in [
            work.snapshot.clone().or_else(|| work.worktree_head.clone()),
            work.branch_tip.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if heads.contains(&candidate) || self.is_contained(&candidate, merged)? {
                continue;
            }
            heads.push(candidate);
        }
        // The kept ref only moves forward: whatever it held stays reachable from it.
        if let Some(existing) = &work.existing_kept
            && !heads.is_empty()
            && !heads.contains(existing)
        {
            heads.push(existing.clone());
        }
        let heads = self.drop_contained_heads(&heads)?;
        let target = match heads.as_slice() {
            [] => None,
            [only] => Some(only.clone()),
            several => Some(self.keep_commit(task, several)?),
        };
        if let Some(target) = &target
            && work.existing_kept.as_ref() != Some(target)
        {
            let kept_ref = task.kept_ref();
            let old = work
                .existing_kept
                .as_ref()
                .map_or_else(String::new, |c| c.as_str().to_owned());
            let message = format!("quartermaster: keep unmerged work of {task}");
            self.run([
                "update-ref",
                "-m",
                message.as_str(),
                kept_ref.as_str(),
                target.as_str(),
                old.as_str(),
            ])?;
            tracing::info!(%task, commit = %target.short(), "unmerged work kept");
        }
        Ok(target.or_else(|| work.existing_kept.clone()))
    }

    /// Fails, before anything is removed, unless every piece of work is reachable from the kept
    /// ref or the merged commits.
    fn verify_reachable(
        &self,
        task: TaskId,
        work: &TaskWork,
        mut safe_set: Vec<CommitSha>,
        kept_commit: Option<&CommitSha>,
    ) -> Result<(), GitError> {
        safe_set.extend(kept_commit.cloned());
        for piece in [&work.snapshot, &work.worktree_head, &work.branch_tip]
            .into_iter()
            .flatten()
        {
            if !self.is_contained(piece, &safe_set)? {
                return Err(GitError::UnexpectedOutput {
                    command: "update-ref".to_owned(),
                    why: format!(
                        "{} is not reachable from {} after saving; nothing was removed",
                        piece.short(),
                        task.kept_ref()
                    ),
                });
            }
        }
        Ok(())
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

    /// Saves the worktree's uncommitted changes (tracked and untracked, not ignored) as a commit
    /// on top of `head`, using a temporary index so the worktree and its own index are untouched.
    /// Returns `None` when there are no changes.
    fn snapshot_uncommitted(
        &self,
        task: TaskId,
        paths: &TaskPaths,
        worktree: &Path,
        head: &CommitSha,
    ) -> Result<Option<CommitSha>, GitError> {
        let index = normalize_path(&paths.root).join(SNAPSHOT_INDEX);
        remove_if_present(&index)?;
        let result = (|| {
            let git = self.git();
            git.invoke(worktree, ["read-tree", head.as_str()])
                .env("GIT_INDEX_FILE", &index)
                .text()?;
            git.invoke(worktree, ["add", "--all", "--", "."])
                .env("GIT_INDEX_FILE", &index)
                .text()?;
            let tree = git
                .invoke(worktree, ["write-tree"])
                .env("GIT_INDEX_FILE", &index)
                .text()?;
            let tree = tree.trim();
            let head_tree = git.run(
                worktree,
                ["rev-parse", &format!("{}^{{tree}}", head.as_str())],
            )?;
            if tree == head_tree.trim() {
                return Ok(None);
            }
            let message = format!(
                "quartermaster: uncommitted work in {task} when it closed\n\nSaved by the app \
                 before retiring the worktree; this commit was never on a branch."
            );
            let commit = self
                .app_commit(worktree)
                .args([
                    "commit-tree",
                    "--no-gpg-sign",
                    tree,
                    "-p",
                    head.as_str(),
                    "-m",
                    &message,
                ])
                .text()?;
            parse_sha("commit-tree", commit.trim()).map(Some)
        })();
        remove_if_present(&index)?;
        result
    }

    /// A keep commit whose parents are every unrelated head, so the kept ref holds them all. Its
    /// tree is the first head's (the most recent work).
    fn keep_commit(&self, task: TaskId, heads: &[CommitSha]) -> Result<CommitSha, GitError> {
        let tree = format!("{}^{{tree}}", heads[0].as_str());
        let message = format!(
            "quartermaster: kept work of {task}\n\nEach parent is a separate piece of unmerged \
             work the task left behind."
        );
        let mut args: Vec<String> = vec!["commit-tree".into(), "--no-gpg-sign".into(), tree];
        for head in heads {
            args.push("-p".into());
            args.push(head.as_str().to_owned());
        }
        args.push("-m".into());
        args.push(message);
        let commit = self.app_commit(self.root()).args(args).text()?;
        parse_sha("commit-tree", commit.trim())
    }

    /// The commits whose history counts as merged.
    fn merged_commits(&self, spec: &RetireSpec<'_>) -> Result<Vec<CommitSha>, GitError> {
        let mut merged: Vec<CommitSha> = spec.base_commit.cloned().into_iter().collect();
        for rev in spec.merged_into {
            if rev.starts_with('-') {
                continue;
            }
            if let Some(commit) = self.resolve_commit(rev)?
                && !merged.contains(&commit)
            {
                merged.push(commit);
            }
        }
        Ok(merged)
    }

    /// Whether `commit` is reachable from any of `from`.
    fn is_contained(&self, commit: &CommitSha, from: &[CommitSha]) -> Result<bool, GitError> {
        for other in from {
            if self.is_ancestor(commit, other)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Drops every head reachable from another one, keeping the order of the rest.
    fn drop_contained_heads(&self, heads: &[CommitSha]) -> Result<Vec<CommitSha>, GitError> {
        let mut kept = Vec::new();
        for (i, head) in heads.iter().enumerate() {
            let mut contained = false;
            for (j, other) in heads.iter().enumerate() {
                if i != j && self.is_ancestor(head, other)? {
                    contained = true;
                    break;
                }
            }
            if !contained {
                kept.push(head.clone());
            }
        }
        Ok(kept)
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
            // Git refuses some worktrees (for example with submodules checked out). The work is
            // already saved and the folder is the task's own, so remove it directly.
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

    /// A `commit-tree` invocation with the app's own identity.
    fn app_commit(&self, dir: &Path) -> AppCommit<'_> {
        AppCommit {
            repo: self,
            dir: dir.to_owned(),
            args: Vec::new(),
        }
    }
}

/// Every piece of work a task could have left behind, read before anything is removed.
#[derive(Debug)]
struct TaskWork {
    /// The worktree's `HEAD`, if the worktree is there.
    worktree_head: Option<CommitSha>,
    /// A commit of the worktree's uncommitted changes on top of its `HEAD`, if there were any.
    snapshot: Option<CommitSha>,
    /// The task branch's tip, if the branch exists.
    branch_tip: Option<CommitSha>,
    /// What the kept ref held before this retirement.
    existing_kept: Option<CommitSha>,
}

/// Builder for a commit the app makes itself (snapshot or keep commit).
struct AppCommit<'a> {
    repo: &'a Repo,
    dir: PathBuf,
    args: Vec<String>,
}

impl AppCommit<'_> {
    fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    fn text(self) -> Result<String, GitError> {
        let (name, email) = APP_IDENTITY;
        self.repo
            .git()
            .invoke(&self.dir, &self.args)
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .text()
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

fn remove_if_present(path: &Path) -> Result<(), GitError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(GitError::io("remove", path, e)),
        _ => Ok(()),
    }
}
