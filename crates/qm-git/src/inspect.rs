//! Reading a task's work: its head, its commit messages and the files it changed.
//!
//! For the crewmate's done report (the head commit is recorded and every commit name checked
//! against the naming rules) and for a review's brief (every changed file). Read-only: nothing
//! here changes a repository.

use std::path::{Path, PathBuf};

use qm_types::ids::CommitSha;

use crate::error::GitError;
use crate::repo::{Repo, normalize_path, parse_sha};

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

    /// Every commit in the worktree at `worktree` since `base` (reachable from its `HEAD` but not
    /// from `base`), oldest first, each with its full message (subject and body, trailers included,
    /// trailing whitespace trimmed). Merge commits are included. These are the names the done
    /// report's commit check reads.
    pub fn commit_messages(
        &self,
        worktree: &Path,
        base: &CommitSha,
    ) -> Result<Vec<(CommitSha, String)>, GitError> {
        let worktree = normalize_path(worktree);
        let range = format!("{}..HEAD", base.as_str());
        let out = self.git().run(
            &worktree,
            [
                "log",
                "--reverse",
                "--no-show-signature",
                "--format=%H%x00%B%x1e",
                range.as_str(),
                "--",
            ],
        )?;
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
