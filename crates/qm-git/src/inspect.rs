//! Reading a task's work: its head, its commit messages and the files it changed.
//!
//! For the commit-name check at `pr_open`, the done report, settling and landing in place (the
//! task's own commits, [`Repo::own_commits`]) and for a review's brief (every changed file). Read-only: nothing
//! here changes a repository.

use std::path::{Path, PathBuf};

use qm_types::ids::CommitSha;

use crate::error::{BaseRefusal, GitError};
use crate::repo::{ORIGIN, Repo, normalize_path, parse_sha};

/// A task's own commits, as [`Repo::own_commits`] found them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnCommits {
    /// The ref the merge-base was taken against: `origin/<branch>`, or the local branch when the
    /// remote ref is missing.
    pub against: String,
    /// Where the task's commits start (exclusive).
    pub merge_base: CommitSha,
    /// The commits, oldest first, merges left out, each with its full message.
    pub commits: Vec<(CommitSha, String)>,
}

/// Separates commits in `git log` output (ASCII record separator).
const RECORD: char = '\u{1e}';

impl Repo {
    /// The commit checked out in the worktree at `worktree`, or `None` if it has none yet.
    ///
    /// Fails with [`GitError::NotAWorktree`] unless git, run in that folder, sees exactly that
    /// worktree, so a folder that lost its `.git` file is never mistaken for a repository further
    /// up the tree.
    pub fn worktree_head(&self, worktree: &Path) -> Result<Option<CommitSha>, GitError> {
        let worktree = normalize_path(worktree);
        let top = self
            .git()
            .invoke(&worktree, ["rev-parse", "--show-toplevel"])
            .output()?;
        if !top.success() || normalize_path(Path::new(top.stdout.trim())) != worktree {
            return Err(GitError::NotAWorktree { path: worktree });
        }
        let out = self
            .git()
            .invoke(
                &worktree,
                ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            )
            .output()?;
        match out.code {
            Some(0) => parse_sha("rev-parse HEAD", out.stdout.trim()).map(Some),
            Some(1) if out.stderr.is_empty() => Ok(None),
            code => Err(GitError::Failed {
                command: "rev-parse --verify HEAD".to_owned(),
                code,
                stderr: out.stderr,
            }),
        }
    }

    /// The task's own commits up to `head`, the ones the commit-name check reads:
    /// `git log --no-merges <merge-base>..head`, oldest first, each with its full message (subject
    /// and body, trailers included, trailing whitespace trimmed).
    ///
    /// The merge-base is taken against `origin/<branch>`, fetched first, so a branch rebased onto
    /// it doesn't count commits already there; only when that remote ref is missing is it taken
    /// against the local `branch`. A fetch that fails is logged and the ref as it stands is used.
    /// `branch` is the branch the task's work targets: the project's default unless the task was
    /// started on another base branch.
    pub fn own_commits(&self, head: &CommitSha, branch: &str) -> Result<OwnCommits, GitError> {
        if !self.is_valid_branch_name(branch)? {
            return Err(BaseRefusal::InvalidBranchName {
                branch: branch.to_owned(),
            }
            .into());
        }
        if self.remotes()?.iter().any(|r| r == ORIGIN)
            && let Err(e) = self.fetch_branch(ORIGIN, branch)
        {
            tracing::warn!("couldn't fetch {ORIGIN}/{branch} before reading a task's commits: {e}");
        }
        let remote = format!("{ORIGIN}/{branch}");
        let (against, tip) = match self.resolve_commit(&format!("refs/remotes/{remote}"))? {
            Some(tip) => (remote, tip),
            None => match self.local_branch_commit(branch)? {
                Some(tip) => (branch.to_owned(), tip),
                None => {
                    return Err(GitError::NoTargetBranch {
                        branch: branch.to_owned(),
                    });
                }
            },
        };
        let merge_base = self
            .merge_base(head, &tip)?
            .ok_or_else(|| GitError::NoMergeBase {
                head: head.clone(),
                against: against.clone(),
            })?;
        let range = format!("{}..{}", merge_base.as_str(), head.as_str());
        let out = self.run([
            "log",
            "--no-merges",
            "--reverse",
            "--no-show-signature",
            "--format=%H%x00%B%x1e",
            range.as_str(),
            "--",
        ])?;
        Ok(OwnCommits {
            against,
            merge_base,
            commits: parse_messages(&out)?,
        })
    }

    /// The files that differ between `base` and `head`, as paths relative to the repository's
    /// top folder, sorted: added, modified and deleted files, and both sides of a rename. The
    /// comparison starts from where `head` branched off `base` (`base...head`), so a base that
    /// moved on doesn't add its own changes. For a review's brief.
    pub fn changed_files(
        &self,
        base: &CommitSha,
        head: &CommitSha,
    ) -> Result<Vec<PathBuf>, GitError> {
        let range = format!("{}...{}", base.as_str(), head.as_str());
        let out = self.run([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            range.as_str(),
            "--",
        ])?;
        let mut files: Vec<PathBuf> = out
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(PathBuf::from)
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }
}

/// Parses `git log --format=%H%x00%B%x1e` output into commits and their messages.
fn parse_messages(out: &str) -> Result<Vec<(CommitSha, String)>, GitError> {
    out.split(RECORD)
        .map(|record| record.trim_start_matches('\n'))
        .filter(|record| !record.trim().is_empty())
        .map(|record| {
            let (sha, message) =
                record
                    .split_once('\0')
                    .ok_or_else(|| GitError::UnexpectedOutput {
                        command: "log".to_owned(),
                        why: "a commit without a separator".to_owned(),
                    })?;
            Ok((parse_sha("log", sha)?, message.trim_end().to_owned()))
        })
        .collect()
}
