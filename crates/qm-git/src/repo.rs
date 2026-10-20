//! A project's repository: your checkout, as git sees it.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use qm_types::ids::CommitSha;

use crate::error::GitError;
use crate::git::Git;
use crate::lock;

/// The remote the default and typed base branches start from.
pub const ORIGIN: &str = "origin";

/// A project's git repository, opened from your checkout (a project is a local git repository
/// with settings).
///
/// Every operation of this crate on a project starts from a `Repo`. Operations that change the
/// repository hold an in-process lock per repository, so the app's own commands never collide.
#[derive(Clone, Debug)]
pub struct Repo {
    git: Git,
    root: PathBuf,
    common_dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

/// One worktree git knows about (`git worktree list --porcelain`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Where git says the worktree is (normalised, so it compares with the app's own paths).
    pub path: PathBuf,
    /// The commit checked out, absent in a repository with no commits yet.
    pub head: Option<CommitSha>,
    /// The branch checked out, as a short name (`qm/task-0042`), or `None` when detached.
    pub branch: Option<String>,
    /// Whether this is your checkout itself rather than a linked worktree.
    pub is_main: bool,
    /// Whether this entry is a bare repository.
    pub bare: bool,
    /// Set when someone locked the worktree; the reason, possibly empty.
    pub locked: Option<String>,
    /// Set when git considers the worktree prunable (its folder is gone); the reason.
    pub prunable: Option<String>,
}

impl Repo {
    /// Opens the repository containing `path`. A path inside a linked worktree opens the
    /// repository it belongs to, rooted at the main checkout.
    pub fn open(git: Git, path: &Path) -> Result<Self, GitError> {
        let out = git
            .invoke(
                path,
                [
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-common-dir",
                    "--is-bare-repository",
                ],
            )
            .output()?;
        if !out.success() {
            return Err(GitError::NotARepository {
                path: path.to_owned(),
            });
        }
        let mut lines = out.stdout.lines();
        let (Some(common_dir), Some(bare)) = (lines.next(), lines.next()) else {
            return Err(unexpected("rev-parse --git-common-dir", "too few lines"));
        };
        let common_dir = normalize_path(Path::new(common_dir));
        let root = if bare.trim() == "true" {
            common_dir.clone()
        } else {
            // The main worktree is the common directory's parent for a normal checkout; ask git
            // to be sure (it may be configured elsewhere with core.worktree).
            let listed = list_worktrees(&git, path)?;
            listed.into_iter().find(|w| w.is_main).map_or_else(
                || {
                    common_dir
                        .parent()
                        .map_or_else(|| common_dir.clone(), Path::to_owned)
                },
                |w| w.path,
            )
        };
        let lock = lock::repo_lock(&common_dir);
        Ok(Self {
            git,
            root,
            common_dir,
            lock,
        })
    }

    /// Your checkout's top folder (the main worktree).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository's common git directory (`.git` of your checkout).
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The runner this repository uses.
    pub fn git(&self) -> &Git {
        &self.git
    }

    /// The names of the repository's remotes, sorted.
    pub fn remotes(&self) -> Result<Vec<String>, GitError> {
        let out = self.run(["remote"])?;
        let mut remotes: Vec<String> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        remotes.sort();
        Ok(remotes)
    }

    /// The project's default branch as far as the local repository knows, without touching the
    /// network: what `origin/HEAD` points at, else (with no remote-tracking default) the configured
    /// `init.defaultBranch` if it exists, else `main` or `master` if one exists, else the branch
    /// your checkout has checked out. For the project registry to record when a project is linked;
    /// settling a base asks the remote itself.
    pub fn detect_default_branch(&self) -> Result<Option<String>, GitError> {
        let origin_head = self
            .git
            .invoke(
                &self.root,
                ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            )
            .output()?;
        if origin_head.success()
            && let Some(name) = origin_head
                .stdout
                .trim()
                .strip_prefix("refs/remotes/origin/")
        {
            return Ok(Some(name.to_owned()));
        }
        let configured = self
            .git
            .invoke(&self.root, ["config", "--get", "init.defaultBranch"])
            .output()?;
        let mut candidates = Vec::new();
        if configured.success() {
            candidates.push(configured.stdout.trim().to_owned());
        }
        candidates.extend(["main".to_owned(), "master".to_owned()]);
        for candidate in candidates {
            if !candidate.is_empty() && self.local_branch_commit(&candidate)?.is_some() {
                return Ok(Some(candidate));
            }
        }
        self.current_branch()
    }

    /// The branch your checkout has checked out, or `None` when its HEAD is detached.
    pub fn current_branch(&self) -> Result<Option<String>, GitError> {
        let out = self
            .git
            .invoke(&self.root, ["symbolic-ref", "--quiet", "--short", "HEAD"])
            .output()?;
        Ok(out
            .success()
            .then(|| out.stdout.trim().to_owned())
            .filter(|b| !b.is_empty()))
    }

    /// Whether your checkout's working tree is clean: no staged, unstaged or untracked changes
    /// (ignored files don't count). Build-in-place work lands only into a clean tree.
    pub fn is_clean(&self) -> Result<bool, GitError> {
        let out = self.run(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])?;
        Ok(out.is_empty())
    }

    /// Every worktree git knows about, your checkout first.
    pub fn worktrees(&self) -> Result<Vec<WorktreeEntry>, GitError> {
        list_worktrees(&self.git, &self.root)
    }

    /// The commit a local branch points at, if the branch exists.
    pub fn local_branch_commit(&self, branch: &str) -> Result<Option<CommitSha>, GitError> {
        self.resolve_commit(&format!("refs/heads/{branch}"))
    }

    /// The commit a full ref (or any revision) resolves to, or `None` if it doesn't resolve.
    pub(crate) fn resolve_commit(&self, rev: &str) -> Result<Option<CommitSha>, GitError> {
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .git
            .invoke(&self.root, ["rev-parse", "--verify", "--quiet", &spec])
            .output()?;
        if !out.success() {
            return Ok(None);
        }
        parse_sha("rev-parse --verify", out.stdout.trim()).map(Some)
    }

    /// Whether `ancestor` is reachable from `descendant` (equal counts).
    pub(crate) fn is_ancestor(
        &self,
        ancestor: &CommitSha,
        descendant: &CommitSha,
    ) -> Result<bool, GitError> {
        if ancestor == descendant {
            return Ok(true);
        }
        let out = self
            .git
            .invoke(
                &self.root,
                [
                    "merge-base",
                    "--is-ancestor",
                    ancestor.as_str(),
                    descendant.as_str(),
                ],
            )
            .output()?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            code => Err(GitError::Failed {
                command: "merge-base --is-ancestor".to_owned(),
                code,
                stderr: out.stderr,
            }),
        }
    }

    /// Every ref under `prefixes` with the commit it points at.
    pub(crate) fn refs_under(
        &self,
        prefixes: &[&str],
    ) -> Result<Vec<(String, CommitSha)>, GitError> {
        let mut args = vec![
            "for-each-ref".to_owned(),
            "--format=%(refname)%00%(objectname)".to_owned(),
        ];
        args.extend(prefixes.iter().map(|p| (*p).to_owned()));
        let out = self.run(&args)?;
        out.lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let (name, sha) = line
                    .split_once('\0')
                    .ok_or_else(|| unexpected("for-each-ref", "a line without a separator"))?;
                Ok((name.to_owned(), parse_sha("for-each-ref", sha)?))
            })
            .collect()
    }

    /// Runs `git <args>` in your checkout; a non-zero exit is an error.
    pub(crate) fn run<I, S>(&self, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.git.run(&self.root, args)
    }

    /// Holds this repository's in-process write lock.
    pub(crate) fn write_lock(&self) -> MutexGuard<'_, ()> {
        lock::hold(&self.lock)
    }
}

fn list_worktrees(git: &Git, dir: &Path) -> Result<Vec<WorktreeEntry>, GitError> {
    let out = git.run(dir, ["worktree", "list", "--porcelain", "-z"])?;
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in out.split('\0') {
        if field.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: normalize_path(Path::new(value)),
                head: None,
                branch: None,
                is_main: entries.is_empty(),
                bare: false,
                locked: None,
                prunable: None,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            return Err(unexpected(
                "worktree list",
                "a field before the first worktree",
            ));
        };
        match key {
            "HEAD" => {
                // An unborn HEAD is printed as all zeros.
                if !value.bytes().all(|b| b == b'0') {
                    entry.head = Some(parse_sha("worktree list", value)?);
                }
            }
            "branch" => {
                entry.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                );
            }
            "bare" => entry.bare = true,
            "locked" => entry.locked = Some(value.to_owned()),
            "prunable" => entry.prunable = Some(value.to_owned()),
            // "detached" and anything a newer git adds.
            _ => {}
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries)
}

pub(crate) fn parse_sha(command: &str, text: &str) -> Result<CommitSha, GitError> {
    CommitSha::from_str(text.trim()).map_err(|e| unexpected(command, &e.to_string()))
}

pub(crate) fn unexpected(command: &str, why: &str) -> GitError {
    GitError::UnexpectedOutput {
        command: command.to_owned(),
        why: why.to_owned(),
    }
}

/// An absolute path with symlinks resolved as far as it exists (`/tmp` → `/private/tmp` on
/// macOS), so paths git prints compare equal to the app's own. A missing tail is kept as given,
/// minus `.` components.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut existing = path.to_owned();
    let mut tail = Vec::new();
    while let Some(name) = existing.file_name().map(ToOwned::to_owned) {
        tail.push(name);
        if !existing.pop() {
            break;
        }
        if let Ok(real) = existing.canonicalize() {
            let mut result = real;
            for name in tail.iter().rev() {
                result.push(name);
            }
            return result;
        }
    }
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}
