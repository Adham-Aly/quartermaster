//! Matching worktrees on disk against tasks at startup.
//!
//! The core passes in what it believes: for each project, the tasks that should have a live
//! worktree right now. This module looks at what git and the data directory actually hold and
//! reports every difference. It deletes nothing, ever: anything unexpected is reported for you
//! (or the Quartermaster) to decide on.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use qm_types::ids::{CommitSha, ProjectId, TaskId};

use crate::error::GitError;
use crate::git::Git;
use crate::repo::{Repo, normalize_path};

/// What the core believes about one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileProject {
    /// The project.
    pub project: ProjectId,
    /// Your checkout (`projects.path`).
    pub checkout: PathBuf,
    /// The tasks that should have a worktree now (started, past `preparing`, and not yet
    /// closed), each with the branch recorded for it.
    pub tasks_with_worktrees: BTreeMap<TaskId, String>,
}

/// Everything startup reconciliation found. Empty lists mean disk and tasks agree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Worktrees the app made (under the data directory's `tasks/`, or on an old-style
    /// `qm/task-*` branch) that no task accounts for.
    pub unexpected_worktrees: Vec<UnexpectedWorktree>,
    /// Tasks that should have a worktree but don't.
    pub missing_worktrees: Vec<MissingWorktree>,
    /// Worktrees git still lists whose folder is gone.
    pub stale_registrations: Vec<StaleRegistration>,
    /// Old-style `qm/task-*` branches with no worktree and no task that should have one. A
    /// branch named after its work isn't recognised by name, so it's never listed here.
    pub orphan_branches: Vec<OrphanBranch>,
    /// Kept refs earlier versions saved closed tasks' unmerged work to; deleted now, their commits
    /// left for git to garbage-collect.
    pub removed_kept_refs: Vec<KeptRef>,
    /// Projects whose repository couldn't be read.
    pub unreadable_projects: Vec<UnreadableProject>,
}

impl ReconcileReport {
    /// Whether anything needs your attention (removed kept refs alone don't).
    pub fn needs_attention(&self) -> bool {
        !(self.unexpected_worktrees.is_empty()
            && self.missing_worktrees.is_empty()
            && self.stale_registrations.is_empty()
            && self.orphan_branches.is_empty()
            && self.unreadable_projects.is_empty())
    }
}

/// A worktree no task accounts for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnexpectedWorktree {
    /// Where it is.
    pub path: PathBuf,
    /// The project whose repository lists it, if any does.
    pub project: Option<ProjectId>,
    /// The task its path or branch names, if any.
    pub task: Option<TaskId>,
    /// Its branch, if it has one checked out.
    pub branch: Option<String>,
    /// Why it's unexpected.
    pub reason: UnexpectedReason,
}

/// Why a worktree is unexpected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnexpectedReason {
    /// Its task isn't one that should have a worktree (closed, unknown, or of another project).
    NoLiveTask,
    /// It's at a task's worktree path but on another branch than the task's.
    WrongBranch {
        /// The task's branch.
        expected: String,
    },
    /// It's on an old-style task branch but outside the data directory's `tasks/` folder.
    OutsideTasksFolder,
    /// A folder at a task's worktree path that no project's repository lists.
    NotRegistered,
}

/// A task that should have a worktree but doesn't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingWorktree {
    pub project: ProjectId,
    pub task: TaskId,
    /// Where it should be.
    pub expected_path: PathBuf,
    /// The task's recorded branch.
    pub branch: String,
    /// Whether the task's branch still exists (its committed work is safe).
    pub branch_exists: bool,
}

/// A worktree git lists whose folder is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleRegistration {
    pub project: ProjectId,
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// An old-style task branch (`qm/task-0042`) with no worktree and no live task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanBranch {
    pub project: ProjectId,
    pub task: TaskId,
    pub branch: String,
    pub commit: CommitSha,
}

/// A kept ref an earlier version saved a closed task's unmerged work to, and startup deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptRef {
    pub project: ProjectId,
    /// The full ref name, `refs/quartermaster/kept/T-0042`.
    pub git_ref: String,
    /// The commit it pointed at.
    pub commit: CommitSha,
}

/// A project whose repository couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreadableProject {
    pub project: ProjectId,
    pub checkout: PathBuf,
    /// Why, as a plain sentence.
    pub reason: String,
}

/// The ref namespace earlier versions saved a closed task's unmerged work in. Nothing writes it
/// now; startup deletes what is there.
const LEGACY_KEPT_PREFIX: &str = "refs/quartermaster/kept";

impl Repo {
    /// Every legacy kept ref in this repository, with the commit it points at, by name.
    pub fn legacy_kept_refs(&self) -> Result<Vec<(String, CommitSha)>, GitError> {
        let prefix = format!("{LEGACY_KEPT_PREFIX}/");
        let mut refs = self.refs_under(&[LEGACY_KEPT_PREFIX])?;
        refs.retain(|(name, _)| name.starts_with(&prefix));
        refs.sort();
        Ok(refs)
    }

    /// Deletes `git_ref` only while it still points at `commit` (compare-and-swap).
    pub fn delete_ref(&self, git_ref: &str, commit: &CommitSha) -> Result<(), GitError> {
        let _guard = self.write_lock();
        self.run(["update-ref", "-d", git_ref, commit.as_str()])?;
        Ok(())
    }

    /// The task numbers this repository's old-style task branches name: local `qm/task-*`
    /// branches and their remote-tracking copies, whose work may come back under the same number.
    /// Branches named after their work name no number, so they reserve none.
    pub fn legacy_task_numbers(&self) -> Result<Vec<TaskId>, GitError> {
        let mut numbers: Vec<TaskId> = self
            .refs_under(&["refs/heads/qm", "refs/remotes"])?
            .into_iter()
            .filter_map(|(name, _)| {
                let branch = match name.strip_prefix("refs/remotes/") {
                    Some(remote) => remote.split_once('/')?.1,
                    None => name.strip_prefix("refs/heads/")?,
                };
                TaskId::from_legacy_branch(branch).ok()
            })
            .collect();
        numbers.sort();
        numbers.dedup();
        Ok(numbers)
    }

    /// Every old-style task branch (`qm/task-*`) in this repository with its task and commit, by
    /// task.
    pub fn legacy_task_branches(&self) -> Result<Vec<(TaskId, CommitSha)>, GitError> {
        let prefix = format!("refs/heads/{}", TaskId::LEGACY_BRANCH_PREFIX);
        let mut branches: Vec<(TaskId, CommitSha)> = self
            .refs_under(&["refs/heads/qm"])?
            .into_iter()
            .filter_map(|(name, commit)| {
                name.strip_prefix(&prefix)?;
                let short = name.strip_prefix("refs/heads/")?;
                Some((TaskId::from_legacy_branch(short).ok()?, commit))
            })
            .collect();
        branches.sort_by_key(|(task, _)| *task);
        Ok(branches)
    }
}

/// Matches worktrees on disk against tasks, deleting nothing.
///
/// `tasks_dir` is the data directory's `tasks/` folder (`DataDir::tasks_dir`). A project whose
/// repository can't be read is reported, not fatal; only failing to read `tasks_dir` itself is
/// an error.
pub fn reconcile(
    git: &Git,
    tasks_dir: &Path,
    projects: &[ReconcileProject],
) -> Result<ReconcileReport, GitError> {
    let tasks_dir = normalize_path(tasks_dir);
    let mut report = ReconcileReport::default();
    let mut registered: BTreeSet<PathBuf> = BTreeSet::new();

    for project in projects {
        if let Err(error) =
            reconcile_project(git, &tasks_dir, project, &mut report, &mut registered)
        {
            report.unreadable_projects.push(UnreadableProject {
                project: project.project,
                checkout: project.checkout.clone(),
                reason: error.to_string(),
            });
        }
    }

    // Folders at task worktree paths that no repository lists.
    let entries = match fs::read_dir(&tasks_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(GitError::io("read", tasks_dir, e)),
    };
    let mut unregistered = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| GitError::io("read", &tasks_dir, e))?;
        let worktree = normalize_path(&entry.path().join("worktree"));
        if worktree.is_dir() && !registered.contains(&worktree) {
            let task = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<TaskId>().ok());
            unregistered.insert(worktree, task);
        }
    }
    for (path, task) in unregistered {
        report.unexpected_worktrees.push(UnexpectedWorktree {
            path,
            project: None,
            task,
            branch: None,
            reason: UnexpectedReason::NotRegistered,
        });
    }
    Ok(report)
}

fn reconcile_project(
    git: &Git,
    tasks_dir: &Path,
    project: &ReconcileProject,
    report: &mut ReconcileReport,
    registered: &mut BTreeSet<PathBuf>,
) -> Result<(), GitError> {
    let repo = Repo::open(git.clone(), &project.checkout)?;
    let worktrees = repo.worktrees()?;
    let legacy_branches = repo.legacy_task_branches()?;
    let legacy = repo.legacy_kept_refs()?;
    let id = project.project;
    let mut seen: BTreeSet<TaskId> = BTreeSet::new();
    let mut checked_out: BTreeSet<String> = BTreeSet::new();

    for entry in worktrees.iter().filter(|w| !w.is_main) {
        registered.insert(entry.path.clone());
        let path_task = task_of_path(tasks_dir, &entry.path);
        // A recorded task branch, or an old-style one, names its task.
        let branch_task = entry.branch.as_deref().and_then(|b| {
            project
                .tasks_with_worktrees
                .iter()
                .find(|(_, recorded)| recorded.as_str() == b)
                .map(|(task, _)| *task)
                .or_else(|| TaskId::from_legacy_branch(b).ok())
        });
        if path_task.is_none() && branch_task.is_none() {
            // One of your own worktrees: not the app's business.
            continue;
        }
        if let Some(branch) = &entry.branch {
            checked_out.insert(branch.clone());
        }
        let gone = entry.prunable.is_some() || !entry.path.is_dir();
        if gone {
            report.stale_registrations.push(StaleRegistration {
                project: id,
                path: entry.path.clone(),
                branch: entry.branch.clone(),
            });
            continue;
        }
        let unexpected = |reason| UnexpectedWorktree {
            path: entry.path.clone(),
            project: Some(id),
            task: path_task.or(branch_task),
            branch: entry.branch.clone(),
            reason,
        };
        match path_task {
            None => report
                .unexpected_worktrees
                .push(unexpected(UnexpectedReason::OutsideTasksFolder)),
            Some(task) => match project.tasks_with_worktrees.get(&task) {
                None => report
                    .unexpected_worktrees
                    .push(unexpected(UnexpectedReason::NoLiveTask)),
                Some(expected) if entry.branch.as_deref() != Some(expected.as_str()) => {
                    report
                        .unexpected_worktrees
                        .push(unexpected(UnexpectedReason::WrongBranch {
                            expected: expected.clone(),
                        }));
                    seen.insert(task);
                }
                Some(_) => {
                    seen.insert(task);
                }
            },
        }
    }

    for (task, branch) in &project.tasks_with_worktrees {
        if seen.contains(task) {
            continue;
        }
        report.missing_worktrees.push(MissingWorktree {
            project: id,
            task: *task,
            expected_path: tasks_dir.join(task.to_string()).join("worktree"),
            branch: branch.clone(),
            branch_exists: !branch.is_empty() && repo.local_branch_commit(branch)?.is_some(),
        });
    }

    for (task, commit) in legacy_branches {
        let branch = task.legacy_branch();
        let recorded = project.tasks_with_worktrees.values().any(|b| *b == branch);
        if !project.tasks_with_worktrees.contains_key(&task)
            && !recorded
            && !checked_out.contains(&branch)
        {
            report.orphan_branches.push(OrphanBranch {
                project: id,
                task,
                branch,
                commit,
            });
        }
    }

    for (git_ref, commit) in legacy {
        repo.delete_ref(&git_ref, &commit)?;
        tracing::info!(project = %id, %git_ref, commit = %commit.short(), "deleted a kept ref");
        report.removed_kept_refs.push(KeptRef {
            project: id,
            git_ref,
            commit,
        });
    }
    Ok(())
}

/// The task whose worktree path `path` is (`<tasks_dir>/T-0042/worktree`), if it is one.
fn task_of_path(tasks_dir: &Path, path: &Path) -> Option<TaskId> {
    let rest = path.strip_prefix(tasks_dir).ok()?;
    let mut parts = rest.components();
    let task = parts.next()?.as_os_str().to_str()?.parse::<TaskId>().ok()?;
    let leaf = parts.next()?;
    (leaf.as_os_str() == "worktree" && parts.next().is_none()).then_some(task)
}
