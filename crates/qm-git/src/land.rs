//! Landing work: the app's own push with a lease, build in place, and the diff a card shows. It
//! lives in this crate because it needs `Repo`'s private helpers.
//!
//! - [`Repo::push`] pushes exactly a commit to a task branch, with `--force-with-lease=<ref>:<the
//!   app's own last push>`, in the app's environment, and only while the remote still pushes to
//!   exactly the URL the caller checked ([`Repo::push_urls`]).
//! - [`Repo::merge_in_place`] merges exactly the approved commit into exactly the approved branch
//!   of a clean checkout, and leaves everything as it was on any doubt.
//! - [`Repo::merge_base`], [`Repo::diff_stat`] and [`Repo::diff_text`] for cards and the Diff
//!   screen.

use std::path::PathBuf;

use qm_types::ids::CommitSha;

use crate::error::GitError;
use crate::network::{Credentials, RepoTransportSetting};
use crate::repo::{Repo, parse_sha};

/// How the app authenticates its push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushCredential {
    /// Whatever git would use (ssh: your agent, in batch mode).
    Default,
    /// For an `https://` remote, `gh auth git-credential`, so the push and the API act as the same
    /// GitHub identity.
    GhHelper { gh: PathBuf },
}

/// One push.
#[derive(Clone, Debug)]
pub struct PushSpec<'a> {
    pub remote: &'a str,
    /// Where the caller checked `remote` pushes to: the one URL [`Repo::push_urls`] gave. The
    /// push goes ahead only if git still resolves `remote` to exactly this URL and nothing else,
    /// read again under the write lock just before pushing; otherwise
    /// [`PushOutcome::Redirected`] and nothing is pushed.
    pub url: &'a str,
    pub head: &'a CommitSha,
    /// The branch on the remote, `qm/task-0042`.
    pub branch: &'a str,
    /// What the app last pushed there; `None` means the branch must not exist yet.
    pub expected_remote: Option<&'a CommitSha>,
    pub credential: PushCredential,
}

/// What a push did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    /// The remote branch isn't where the app left it: nothing was pushed.
    LeaseRejected {
        remote_now: Option<CommitSha>,
    },
    /// The remote refused it for another reason, with git's words.
    Rejected {
        message: String,
    },
    /// `remote` no longer pushes to exactly [`PushSpec::url`] (these are the URLs it resolves
    /// to now): nothing was pushed.
    Redirected {
        urls: Vec<String>,
    },
    /// The repository's own config (or a file it includes) sets these, which change how a push
    /// travels (a proxy, a certificate authority, another transport) and which git gives the
    /// app no way to override: nothing was pushed.
    RepoConfig {
        settings: Vec<RepoTransportSetting>,
    },
}

/// A merge into your checkout.
#[derive(Clone, Debug)]
pub struct InPlaceMerge<'a> {
    /// The branch your checkout must have checked out.
    pub target_branch: &'a str,
    /// The task branch, `qm/task-0050`.
    pub branch: &'a str,
    /// The approved commit, merged by SHA.
    pub head: &'a CommitSha,
    /// The merge commit's message when it can't fast-forward.
    pub message: &'a str,
}

/// What a merge into your checkout did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InPlaceOutcome {
    Merged {
        new_head: CommitSha,
        fast_forward: bool,
    },
    /// Your working tree has changes; nothing was touched.
    NotClean,
    /// Your checkout is on another branch (or detached); nothing was touched.
    WrongBranch { current: Option<String> },
    /// The task branch isn't at the approved commit any more; nothing was touched.
    HeadMoved { now: Option<CommitSha> },
    /// The merge conflicts in these files; it was aborted and your checkout is as it was.
    Conflict { files: Vec<String> },
}

/// A diff's size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files: u32,
    pub additions: u64,
    pub deletions: u64,
}

impl Repo {
    /// Every URL `git push <remote>` would push to, exactly as git resolves them, in git's
    /// order; empty when `remote` doesn't exist or has no URL.
    ///
    /// `git remote get-url --push --all` prints the remote's push URLs after git has applied
    /// every rewrite: each `remote.<name>.pushurl` with `url.<base>.insteadOf` applied, or, with
    /// no `pushurl`, each `remote.<name>.url` with `url.<base>.pushInsteadOf` (else
    /// `insteadOf`) applied. Git builds that list once when it reads its config, and `git push
    /// <remote>` pushes to every entry of the same list, so with the same configuration the two
    /// can't disagree. (`git ls-remote --get-url` applies `insteadOf` only, never
    /// `pushInsteadOf`, so it can't stand in for this.)
    ///
    /// This matters because a task's worktree shares the repository's `config` with your
    /// checkout: a `git remote set-url --push origin …` or a `url.*.insteadOf` written from any
    /// worktree changes where your checkout, and the app, push.
    pub fn push_urls(&self, remote: &str) -> Result<Vec<String>, GitError> {
        let out = self
            .git()
            .invoke(
                self.root(),
                ["remote", "get-url", "--push", "--all", remote],
            )
            .output()?;
        if !out.success() {
            return Ok(Vec::new());
        }
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// What `refs/heads/<branch>` is on `remote` now, asked over the app's own transport.
    fn remote_branch(
        &self,
        remote: &str,
        branch: &str,
        credentials: Credentials<'_>,
    ) -> Result<Option<CommitSha>, GitError> {
        let reference = format!("refs/heads/{branch}");
        let out = self
            .network_command(
                remote,
                None,
                credentials,
                ["ls-remote", "--refs", remote, reference.as_str()],
            )?
            .text()?;
        out.lines()
            .find_map(|line| {
                let (sha, name) = line.split_once('\t')?;
                (name == reference).then(|| parse_sha("ls-remote", sha))
            })
            .transpose()
    }

    /// Pushes exactly `head` to `refs/heads/<branch>` on `remote`, under the repository's write
    /// lock, as a network command in the app's own environment, hooks skipped like every app
    /// command. The lease is the app's own last push: a crewmate's rebase makes later pushes
    /// non-fast-forward, and the lease is what makes forcing safe.
    ///
    /// The destination is compare-and-push: `remote` must still resolve ([`Repo::push_urls`]) to
    /// exactly `[spec.url]`, the URL the caller checked, or nothing is pushed
    /// ([`PushOutcome::Redirected`]). The URLs are read with the same configuration the push
    /// runs with (the `-c` options below don't touch remotes or URL rewriting). What remains is
    /// a config write landing in the milliseconds between that read and git's own; the caller
    /// confirms on GitHub afterwards before it records anything.
    ///
    /// Why not push to an app-built URL with the repository's rewrites switched off instead: git
    /// has no switch that ignores the repository's own config (`GIT_CONFIG_NOSYSTEM` and
    /// `GIT_CONFIG_GLOBAL` cover the other two levels), `-c` only adds values, and a
    /// `-c url.<U>.insteadOf=<U>` meant to pin `<U>` loses to a repository rule with the same
    /// prefix, since git keeps the first of equally long matches.
    ///
    /// The push travels the app's own transport (`Repo::network_command`, shared with every
    /// app network command); repository config it can't override stops it
    /// ([`PushOutcome::RepoConfig`]).
    pub fn push(&self, spec: &PushSpec<'_>) -> Result<PushOutcome, GitError> {
        let _lock = self.write_lock();
        let urls = self.push_urls(spec.remote)?;
        if urls != [spec.url] {
            return Ok(PushOutcome::Redirected { urls });
        }
        let credentials = match &spec.credential {
            PushCredential::Default => Credentials::Yours,
            PushCredential::GhHelper { gh } => Credentials::Gh(gh),
        };
        let reference = format!("refs/heads/{}", spec.branch);
        // A push names the exact commit and the task branch, never anything to merge.
        let refspec = format!("{}:{reference}", spec.head.as_str());
        let lease = format!(
            "--force-with-lease={reference}:{}",
            spec.expected_remote.map_or("", CommitSha::as_str)
        );
        // The flags after `--no-verify` beat the repository's `push.*` settings: exactly the
        // one ref, no tags, no submodule pushes elsewhere, no signing, and only the lease.
        let args = [
            "push",
            "--porcelain",
            "--no-verify",
            "--no-follow-tags",
            "--no-recurse-submodules",
            "--no-signed",
            "--no-force-if-includes",
            lease.as_str(),
            spec.remote,
            refspec.as_str(),
        ];
        let invocation = match self.network_command(spec.remote, Some(spec.url), credentials, args)
        {
            Err(GitError::RepoTransportConfig { settings, .. }) => {
                return Ok(PushOutcome::RepoConfig { settings });
            }
            other => other?,
        };
        let out = invocation.output()?;
        if out.success() {
            return Ok(PushOutcome::Pushed);
        }
        // Porcelain: `<flag>\t<from>:<to>\t<summary> (<reason>)`, flag `!` for a rejected ref.
        let rejected = out
            .stdout
            .lines()
            .find(|l| l.starts_with('!') && l.contains(&reference));
        match rejected {
            Some(line) if line.contains("stale info") => {
                match self.remote_branch(spec.remote, spec.branch, credentials) {
                    Ok(remote_now) => Ok(PushOutcome::LeaseRejected { remote_now }),
                    Err(GitError::RepoTransportConfig { settings, .. }) => {
                        Ok(PushOutcome::RepoConfig { settings })
                    }
                    Err(e) => Err(e),
                }
            }
            Some(line) => Ok(PushOutcome::Rejected {
                message: [line.trim(), out.stderr.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("; "),
            }),
            None => Err(GitError::Failed {
                command: format!("push {} {}", spec.remote, spec.branch),
                code: out.code,
                stderr: out.stderr,
            }),
        }
    }

    /// Merges exactly `spec.head` into your checkout's `spec.target_branch`, under the repository's
    /// write lock: the checkout must be on that branch, clean, and the task branch at the approved
    /// commit. Fast-forwards when it can, else a merge commit with `spec.message`. A conflict is
    /// aborted, leaving `HEAD` and the working tree exactly as they were.
    pub fn merge_in_place(&self, spec: &InPlaceMerge<'_>) -> Result<InPlaceOutcome, GitError> {
        let _lock = self.write_lock();
        // Exactly the approved branch is checked out.
        let current = self.current_branch()?;
        if current.as_deref() != Some(spec.target_branch) {
            return Ok(InPlaceOutcome::WrongBranch { current });
        }
        // The working tree is clean.
        if !self.is_clean()? {
            return Ok(InPlaceOutcome::NotClean);
        }
        // The task branch is still at the approved commit.
        let now = self.local_branch_commit(spec.branch)?;
        if now.as_ref() != Some(spec.head) {
            return Ok(InPlaceOutcome::HeadMoved { now });
        }
        let before = self.resolve_commit("HEAD")?;
        // Merge the SHA, never the branch name.
        let out = self
            .git()
            .invoke(
                self.root(),
                [
                    "merge",
                    "--ff",
                    "--no-edit",
                    "--no-verify",
                    "-m",
                    spec.message,
                    spec.head.as_str(),
                ],
            )
            .output()?;
        if !out.success() {
            let conflicted = self
                .git()
                .invoke(
                    self.root(),
                    ["diff", "--name-only", "--diff-filter=U", "-z"],
                )
                .output()?;
            let files: Vec<String> = conflicted
                .stdout
                .split('\0')
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect();
            self.restore(before.as_ref())?;
            if files.is_empty() {
                return Err(GitError::Failed {
                    command: format!("merge {}", spec.head.short()),
                    code: out.code,
                    stderr: out.stderr,
                });
            }
            return Ok(InPlaceOutcome::Conflict { files });
        }
        let new_head = self
            .resolve_commit("HEAD")?
            .ok_or_else(|| crate::repo::unexpected("merge", "HEAD doesn't resolve after it"))?;
        // The approved commit is in the new HEAD before it counts.
        if !self.is_ancestor(spec.head, &new_head)? {
            return Err(crate::repo::unexpected(
                "merge",
                "the approved commit isn't in HEAD after the merge",
            ));
        }
        Ok(InPlaceOutcome::Merged {
            fast_forward: &new_head == spec.head,
            new_head,
        })
    }

    /// After a failed merge: abort it and make sure `HEAD` is back where it was. The tree was
    /// clean before the merge, so resetting to the old `HEAD` loses nothing of yours.
    fn restore(&self, before: Option<&CommitSha>) -> Result<(), GitError> {
        let _ = self
            .git()
            .invoke(self.root(), ["merge", "--abort"])
            .output()?;
        let now = self.resolve_commit("HEAD")?;
        if let Some(before) = before
            && now.as_ref() != Some(before)
        {
            self.run(["reset", "--hard", "--quiet", before.as_str()])?;
        }
        Ok(())
    }

    /// Where `a` and `b` branched apart, if they share history.
    pub fn merge_base(&self, a: &CommitSha, b: &CommitSha) -> Result<Option<CommitSha>, GitError> {
        let out = self
            .git()
            .invoke(self.root(), ["merge-base", a.as_str(), b.as_str()])
            .output()?;
        match out.code {
            Some(0) => parse_sha("merge-base", out.stdout.trim()).map(Some),
            Some(1) => Ok(None),
            code => Err(GitError::Failed {
                command: "merge-base".to_owned(),
                code,
                stderr: out.stderr,
            }),
        }
    }

    fn diff_range(&self, base: &CommitSha, head: &CommitSha) -> Result<String, GitError> {
        let from = self.merge_base(base, head)?.unwrap_or_else(|| base.clone());
        Ok(format!("{}..{}", from.as_str(), head.as_str()))
    }

    /// Files changed, lines added and removed, from where `head` branched off `base`.
    pub fn diff_stat(&self, base: &CommitSha, head: &CommitSha) -> Result<DiffStat, GitError> {
        let range = self.diff_range(base, head)?;
        let out = self.run([
            "diff",
            "--numstat",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            range.as_str(),
            "--",
        ])?;
        let mut stat = DiffStat::default();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.split('\t');
            let added = parts.next().and_then(|a| a.parse::<u64>().ok());
            let removed = parts.next().and_then(|d| d.parse::<u64>().ok());
            stat.files += 1;
            stat.additions += added.unwrap_or(0);
            stat.deletions += removed.unwrap_or(0);
        }
        Ok(stat)
    }

    /// The patch from where `head` branched off `base`, cut at `max_lines` lines; `true` when
    /// it was cut.
    pub fn diff_text(
        &self,
        base: &CommitSha,
        head: &CommitSha,
        max_lines: usize,
    ) -> Result<(String, bool), GitError> {
        let range = self.diff_range(base, head)?;
        let out = self.run([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            range.as_str(),
            "--",
        ])?;
        let total = out.lines().count();
        if total <= max_lines {
            return Ok((out, false));
        }
        let mut cut: String = out.lines().take(max_lines).collect::<Vec<_>>().join("\n");
        cut.push('\n');
        Ok((cut, true))
    }
}
