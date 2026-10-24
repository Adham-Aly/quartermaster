//! Creating a task's worktree.
//!
//! The worktree lives at `tasks/C-0042/worktree` (outside your checkout, next to the task's
//! evidence folder, so evidence can never be committed) on the branch recorded with the task
//! (`fix/upload-retry-on-slow-tmp`, named by the core), starting at
//! the settled base commit. The project's listed files are then copied in from your checkout,
//! and its setup command run ([`crate::setup`]).
//!
//! Creation is safe to repeat: after a crash mid-way, calling it again for the same task picks up
//! the worktree that is already there. It never deletes or overwrites anything it didn't create:
//! a foreign folder at the path, or a task branch that already holds other commits, is an error.

use std::fs;
use std::path::{Component, Path, PathBuf};

use qm_types::data_dir::TaskPaths;
use qm_types::ids::CommitSha;

use crate::base::SettledBase;
use crate::error::GitError;
use crate::repo::{Repo, normalize_path};
use crate::setup::{SetupCommand, SetupOutcome, run_setup};

/// Everything needed to prepare a task's worktree.
#[derive(Clone, Copy, Debug)]
pub struct WorktreeSpec<'a> {
    /// The task's branch, as recorded with the task (`fix/upload-retry-on-slow-tmp`).
    pub branch: &'a str,
    /// The task's folders; the worktree goes at `paths.worktree`.
    pub paths: &'a TaskPaths,
    /// Where the branch starts.
    pub base: &'a SettledBase,
    /// The project's files to copy from your checkout, relative to its top folder
    /// (`projects.files_to_copy`).
    pub files_to_copy: &'a [PathBuf],
    /// The project's setup command, if it has one.
    pub setup: Option<&'a SetupCommand>,
}

/// A task's worktree, ready for its crewmate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedWorktree {
    /// Where the worktree is.
    pub path: PathBuf,
    /// Its branch, as recorded with the task.
    pub branch: String,
    /// The commit the branch started at (the settled base).
    pub base_commit: CommitSha,
    /// Whether the worktree already existed from an earlier, interrupted attempt.
    pub reused: bool,
    /// What happened to each listed file.
    pub copied: CopyReport,
    /// The setup command's result, if the project has one.
    pub setup: Option<SetupOutcome>,
}

/// What happened to the project's listed files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Copied from your checkout into the worktree.
    pub copied: Vec<PathBuf>,
    /// Already in the worktree (tracked by git, or copied by an earlier attempt), so left as is.
    pub already_present: Vec<PathBuf>,
    /// Not in your checkout, so nothing to copy (a `.env` that exists on one Mac only).
    pub missing: Vec<PathBuf>,
}

/// A worktree added back on its task's existing branch ([`Repo::reattach_worktree`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReattachedWorktree {
    /// Where the worktree is.
    pub path: PathBuf,
    /// Its branch, as recorded with the task.
    pub branch: String,
    /// The branch's commit, now checked out; unchanged by reattaching.
    pub head: CommitSha,
}

impl Repo {
    /// Adds a task's worktree back on its existing branch after its folder vanished (reconcile's
    /// [`MissingWorktree`](crate::MissingWorktree) with `branch_exists: true`), so the task can
    /// resume. The branch's commits are untouched; only the folder and git's record of it are
    /// recreated. Copied files and the setup command are not redone here: call
    /// [`Repo::copy_files_into`] and [`run_setup`] as needed.
    ///
    /// Refuses with [`GitError::WorktreePathOccupied`] if anything exists at the worktree path,
    /// [`GitError::BranchMissing`] if the branch doesn't exist, and
    /// [`GitError::BranchCheckedOutElsewhere`] if another live worktree has it checked out.
    pub fn reattach_worktree(
        &self,
        branch: &str,
        paths: &TaskPaths,
    ) -> Result<ReattachedWorktree, GitError> {
        let branch = branch.to_owned();
        let path = normalize_path(&paths.worktree);
        let _guard = self.write_lock();
        if fs::symlink_metadata(&path).is_ok() {
            return Err(GitError::WorktreePathOccupied { path, branch });
        }
        let Some(head) = self.local_branch_commit(&branch)? else {
            return Err(GitError::BranchMissing { branch });
        };
        let worktrees = self.worktrees()?;
        if let Some(entry) = worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some(branch.as_str()) && w.path != path)
            .filter(|w| w.prunable.is_none() && w.path.exists())
        {
            return Err(GitError::BranchCheckedOutElsewhere {
                branch,
                path: entry.path.clone(),
            });
        }
        // A stale record of this worktree (or of another, vanished, one on the branch) needs
        // --force; the folder itself is known to be absent.
        let stale = worktrees.iter().any(|w| {
            !w.is_main && (w.path == path || w.branch.as_deref() == Some(branch.as_str()))
        });
        fs::create_dir_all(&paths.root)
            .map_err(|e| GitError::io("create the task folder", &paths.root, e))?;
        let mut args: Vec<std::ffi::OsString> =
            vec!["worktree".into(), "add".into(), "--quiet".into()];
        if stale {
            args.push("--force".into());
        }
        args.extend([path.as_os_str().to_owned(), branch.clone().into()]);
        self.run(&args)?;
        tracing::info!(path = %path.display(), branch, head = %head.short(), "worktree reattached");
        Ok(ReattachedWorktree { path, branch, head })
    }

    /// Copies the project's listed files from your checkout into `worktree`, never overwriting what
    /// is already there; the same copy [`Repo::create_worktree`] does, for a reattached worktree.
    pub fn copy_files_into(
        &self,
        worktree: &Path,
        files_to_copy: &[PathBuf],
    ) -> Result<CopyReport, GitError> {
        for file in files_to_copy {
            validate_copy_path(file)?;
        }
        self.copy_listed_files(&normalize_path(worktree), files_to_copy)
    }

    /// Prepares a task's worktree: creates it on the task's branch from the settled base, copies
    /// the project's listed files and runs its setup command.
    ///
    /// A failing setup command is an error ([`GitError::SetupFailed`]) and leaves the worktree in
    /// place; the core decides whether to retry or retire it ([`Repo::retire`]).
    pub fn prepare_worktree(&self, spec: &WorktreeSpec<'_>) -> Result<PreparedWorktree, GitError> {
        let mut prepared =
            self.create_worktree(spec.branch, spec.paths, spec.base, spec.files_to_copy)?;
        if let Some(setup) = spec.setup {
            prepared.setup = Some(run_setup(&prepared.path, setup)?);
        }
        Ok(prepared)
    }

    /// Creates the worktree and copies the listed files, without running the setup command (for
    /// a core that runs it under its own process supervision).
    pub fn create_worktree(
        &self,
        branch: &str,
        paths: &TaskPaths,
        base: &SettledBase,
        files_to_copy: &[PathBuf],
    ) -> Result<PreparedWorktree, GitError> {
        // Check every listed path before changing anything.
        for file in files_to_copy {
            validate_copy_path(file)?;
        }
        let branch = branch.to_owned();
        fs::create_dir_all(&paths.root)
            .map_err(|e| GitError::io("create the task folder", &paths.root, e))?;
        let path = normalize_path(&paths.worktree);

        let reused = {
            let _guard = self.write_lock();
            self.add_worktree(&path, &branch, &base.commit)?
        };

        let copied = self.copy_listed_files(&path, files_to_copy)?;
        Ok(PreparedWorktree {
            path,
            branch,
            base_commit: base.commit.clone(),
            reused,
            copied,
            setup: None,
        })
    }

    /// Adds the worktree, or finds it already there. Returns whether it was already there.
    fn add_worktree(&self, path: &Path, branch: &str, base: &CommitSha) -> Result<bool, GitError> {
        let worktrees = self.worktrees()?;
        let mut stale_at_path = false;
        if let Some(entry) = worktrees.iter().find(|w| w.path == path) {
            if entry.prunable.is_none() && path.exists() {
                if entry.branch.as_deref() == Some(branch) {
                    tracing::info!(path = %path.display(), branch, "worktree already there");
                    return Ok(true);
                }
                return Err(GitError::WorktreePathOccupied {
                    path: path.to_owned(),
                    branch: branch.to_owned(),
                });
            }
            // Registered, but its folder is gone: git needs --force to reuse the path.
            stale_at_path = true;
        }
        if let Some(entry) = worktrees
            .iter()
            .find(|w| w.path != path && w.branch.as_deref() == Some(branch))
        {
            if entry.prunable.is_none() {
                return Err(GitError::BranchCheckedOutElsewhere {
                    branch: branch.to_owned(),
                    path: entry.path.clone(),
                });
            }
            stale_at_path = true;
        }
        if !is_absent_or_empty_dir(path)? {
            return Err(GitError::WorktreePathOccupied {
                path: path.to_owned(),
                branch: branch.to_owned(),
            });
        }

        let path_arg = path.as_os_str().to_owned();
        let mut args: Vec<std::ffi::OsString> =
            vec!["worktree".into(), "add".into(), "--quiet".into()];
        if stale_at_path {
            args.push("--force".into());
        }
        match self.local_branch_commit(branch)? {
            None => {
                args.extend([
                    "--no-track".into(),
                    "-b".into(),
                    branch.into(),
                    path_arg,
                    base.as_str().into(),
                ]);
            }
            Some(tip) if tip == *base => {
                // An earlier attempt made the branch but not the worktree; nothing on it to lose.
                args.extend([path_arg, branch.into()]);
            }
            Some(tip) => {
                return Err(GitError::BranchExists {
                    branch: branch.to_owned(),
                    commit: tip.as_str().to_owned(),
                });
            }
        }
        self.run(&args)?;
        tracing::info!(path = %path.display(), branch, base = %base.short(), "worktree created");
        Ok(false)
    }

    /// Copies each listed file or folder from your checkout into the worktree, never overwriting
    /// what is already there.
    fn copy_listed_files(
        &self,
        worktree: &Path,
        files: &[PathBuf],
    ) -> Result<CopyReport, GitError> {
        let mut report = CopyReport::default();
        for file in files {
            let source = self.root().join(file);
            let target = worktree.join(file);
            match fs::symlink_metadata(&source) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    report.missing.push(file.clone());
                    continue;
                }
                Err(e) => return Err(GitError::io("read", source, e)),
                Ok(_) => {}
            }
            if fs::symlink_metadata(&target).is_ok() {
                report.already_present.push(file.clone());
                continue;
            }
            ensure_parent_inside(worktree, &target)?;
            copy_entry(&source, &target)?;
            report.copied.push(file.clone());
        }
        Ok(report)
    }
}

/// A listed file must be a plain relative path inside the checkout, outside `.git`.
fn validate_copy_path(path: &Path) -> Result<(), GitError> {
    let invalid = |why| GitError::InvalidCopyPath {
        path: path.to_owned(),
        why,
    };
    if path.as_os_str().is_empty() {
        return Err(invalid("it is empty"));
    }
    let mut normal = 0;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                if normal == 0 && name == ".git" {
                    return Err(invalid("it is inside .git"));
                }
                normal += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(invalid("it leaves the checkout (..)")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("it must be relative to the checkout"));
            }
        }
    }
    if normal == 0 {
        return Err(invalid("it names the checkout itself"));
    }
    Ok(())
}

fn is_absent_or_empty_dir(path: &Path) -> Result<bool, GitError> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(GitError::io("read", path, e)),
        Ok(meta) if meta.is_dir() => {
            let mut entries = fs::read_dir(path).map_err(|e| GitError::io("read", path, e))?;
            Ok(entries.next().is_none())
        }
        Ok(_) => Ok(false),
    }
}

/// Creates `target`'s parent folders and checks they resolve inside the worktree (a tracked
/// symlink must not carry a copy outside it).
fn ensure_parent_inside(worktree: &Path, target: &Path) -> Result<(), GitError> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|e| GitError::io("create", parent, e))?;
    let real_parent = parent
        .canonicalize()
        .map_err(|e| GitError::io("resolve", parent, e))?;
    let real_worktree = worktree
        .canonicalize()
        .map_err(|e| GitError::io("resolve", worktree, e))?;
    if real_parent.starts_with(&real_worktree) {
        Ok(())
    } else {
        Err(GitError::InvalidCopyPath {
            path: target.to_owned(),
            why: "its folder in the worktree is a link that leads outside it",
        })
    }
}

/// Copies a file (keeping its permissions), a symlink (as a symlink) or a folder (recursively,
/// skipping anything already there).
fn copy_entry(source: &Path, target: &Path) -> Result<(), GitError> {
    let meta = fs::symlink_metadata(source).map_err(|e| GitError::io("read", source, e))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        let link = fs::read_link(source).map_err(|e| GitError::io("read", source, e))?;
        std::os::unix::fs::symlink(&link, target).map_err(|e| GitError::io("copy", target, e))?;
    } else if kind.is_dir() {
        fs::create_dir_all(target).map_err(|e| GitError::io("create", target, e))?;
        let entries = fs::read_dir(source).map_err(|e| GitError::io("read", source, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| GitError::io("read", source, e))?;
            let child_target = target.join(entry.file_name());
            if fs::symlink_metadata(&child_target).is_ok() {
                continue;
            }
            copy_entry(&entry.path(), &child_target)?;
        }
    } else if kind.is_file() {
        fs::copy(source, target).map_err(|e| GitError::io("copy", target, e))?;
    }
    // Sockets, fifos and devices are not copied.
    Ok(())
}
