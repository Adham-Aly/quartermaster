//! What can go wrong in `qm-git`, as sentences a person (or an agent) can act on.
//!
//! Following the workspace's error policy (`qm_types::error`), every failure is a [`GitError`].
//! A base-branch refusal is a rule saying "no", not a malfunction, so it has its own enum,
//! [`BaseRefusal`], which converts into a [`Refusal`] carrying the same reason.

use std::path::PathBuf;
use std::time::Duration;

use qm_types::domain::DeliveryMode;
use qm_types::error::Refusal;
use qm_types::ids::CommitSha;

use crate::network::RepoTransportSetting;

/// Everything a `qm-git` operation can fail with. Each message is a plain reason that can be
/// shown to you or handed to the supervisor as is; none carries a secret.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitError {
    /// The installed `git` couldn't be started at all.
    #[error("couldn't run git ({program}): {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A git command exited unsuccessfully. `stderr` is git's own explanation, trimmed.
    #[error("`git {command}` failed{}: {}", exit_suffix(*.code), detail_or(.stderr, "git gave no reason"))]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },

    /// A git command ran past its time limit and was killed.
    #[error("`git {command}` didn't finish within {}s and was stopped", .after.as_secs())]
    TimedOut { command: String, after: Duration },

    /// Git printed something this wrapper couldn't parse; a bug here or an unexpected git.
    #[error("couldn't read the output of `git {command}`: {why}")]
    UnexpectedOutput { command: String, why: String },

    /// The folder isn't inside a git repository.
    #[error("{} is not a git repository", .path.display())]
    NotARepository { path: PathBuf },

    /// The repository's own config sets something that would change how an app network command
    /// travels and that the app can't override (a proxy, a certificate authority, another
    /// transport), so the command wasn't run. Task worktrees share that config.
    #[error(
        "the git config of {} sets {}, which would change how the app's git talks to {remote} \
         and which the app can't override (task worktrees share that config), so the app didn't \
         contact {remote}; remove {} with {}",
        .checkout.display(),
        RepoTransportSetting::listed(.settings),
        it_or_them(.settings),
        RepoTransportSetting::removal(.settings)
    )]
    RepoTransportConfig {
        checkout: PathBuf,
        remote: String,
        settings: Vec<RepoTransportSetting>,
    },

    /// Where a task starts was refused under the base-branch rule; the task must not start.
    #[error(transparent)]
    BaseRefused(#[from] BaseRefusal),

    /// Something already sits where the task's worktree belongs, and it isn't that task's
    /// worktree. Nothing there is touched.
    #[error(
        "{} already exists and isn't the worktree of {branch}; it was left untouched",
        .path.display()
    )]
    WorktreePathOccupied { path: PathBuf, branch: String },

    /// The task's branch already exists with commits of its own, so starting the task again would
    /// either lose them or build on them silently.
    #[error(
        "branch {branch} already exists at {} with work not on the base; it was left untouched",
        &.commit[..7.min(.commit.len())]
    )]
    BranchExists { branch: String, commit: String },

    /// The task's branch doesn't exist, so there's nothing to reattach a worktree to.
    #[error("branch {branch} doesn't exist, so its worktree can't be added back")]
    BranchMissing { branch: String },

    /// Git, run in this folder, doesn't see it as the top of a worktree (it may be a plain folder
    /// inside some other repository).
    #[error("{} is not the top folder of a git worktree", .path.display())]
    NotAWorktree { path: PathBuf },

    /// Neither `origin/<branch>` nor the local `branch` exists, so the task's own commits can't be
    /// told from the ones it builds on.
    #[error(
        "neither origin/{branch} nor {branch} exists, so the task's own commits can't be found"
    )]
    NoTargetBranch { branch: String },

    /// The task's head shares no history with the branch it targets.
    #[error("{} shares no history with {against}, so the task's own commits can't be found", .head.short())]
    NoMergeBase { head: CommitSha, against: String },

    /// The task's branch is checked out somewhere other than the task's own worktree.
    #[error("branch {branch} is checked out at {}; it was left untouched", .path.display())]
    BranchCheckedOutElsewhere { branch: String, path: PathBuf },

    /// A file the project lists to copy into worktrees isn't a plain relative path inside the
    /// checkout.
    #[error("the file to copy {path:?} isn't allowed: {why}")]
    InvalidCopyPath { path: PathBuf, why: &'static str },

    /// A filesystem operation failed.
    #[error("couldn't {action} {}: {source}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The project's setup command couldn't be started.
    #[error("couldn't start the setup command `{command}`: {source}")]
    SetupSpawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// The project's setup command exited unsuccessfully. `output` is the end of what it printed
    /// (stdout and stderr interleaved as captured).
    #[error("the setup command `{command}` failed{}: {}", exit_suffix(*.code), detail_or(.output, "it printed nothing"))]
    SetupFailed {
        command: String,
        code: Option<i32>,
        output: String,
    },

    /// The project's setup command ran past its time limit and was killed.
    #[error("the setup command `{command}` didn't finish within {}s and was stopped", .after.as_secs())]
    SetupTimedOut {
        command: String,
        after: Duration,
        output: String,
    },
}

/// Why a task's base was refused. Each message says what was wrong, so the supervisor can tell you,
/// or fix what it can.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BaseRefusal {
    /// A typed base branch that doesn't trace back to your own message.
    #[error(
        "the base branch {branch} doesn't trace back to the user's own message; only a branch \
         the user names can change where a task starts"
    )]
    NotTracedToUser { branch: String },

    /// A typed base branch on a project whose delivery isn't a pull request.
    #[error(
        "a base branch ({branch}) can only be used when the project delivers by pull request, \
         and this project's delivery is {}",
        delivery_label(*.delivery)
    )]
    NeedsPullRequest {
        branch: String,
        delivery: DeliveryMode,
    },

    /// A typed base branch on a project with no remote.
    #[error("a base branch ({branch}) needs a remote, and this project has none")]
    NoRemote { branch: String },

    /// A typed base branch the remote doesn't have.
    #[error("the remote {remote} has no branch named {branch}")]
    NotOnRemote { remote: String, branch: String },

    /// A name git wouldn't accept as a branch.
    #[error("{branch:?} is not a valid branch name")]
    InvalidBranchName { branch: String },

    /// The project has remotes, but none named `origin`, which the default base starts from.
    #[error(
        "the project has no remote named origin (it has: {}); the task is refused rather than \
         guessing which remote to start from",
        .remotes.join(", ")
    )]
    NoOriginRemote { remotes: Vec<String> },

    /// The remote's default branch couldn't be found.
    #[error(
        "couldn't find the default branch of {remote}: {}; the task is refused rather than built \
         on stale code",
        detail_or(.detail, "the remote didn't say")
    )]
    DefaultBranchUnknown { remote: String, detail: String },

    /// Fetching the base failed (network, credentials, a missing repository).
    #[error(
        "couldn't fetch {branch} from {remote}: {}; the task is refused rather than built on \
         stale code",
        detail_or(.detail, "git gave no reason")
    )]
    FetchFailed {
        remote: String,
        branch: String,
        detail: String,
    },

    /// The repository's own config sets something that would change how the app's fetch
    /// travels and that the app can't override, so nothing was fetched (the network form of
    /// [`GitError::RepoTransportConfig`]).
    #[error(
        "the git config of {} sets {}, which would change how the app fetches from {remote} and \
         which the app can't override (task worktrees share that config), so nothing was \
         fetched; remove {} with {}, then start the task again",
        .checkout.display(),
        RepoTransportSetting::listed(.settings),
        it_or_them(.settings),
        RepoTransportSetting::removal(.settings)
    )]
    RepoTransportConfig {
        checkout: PathBuf,
        remote: String,
        settings: Vec<RepoTransportSetting>,
    },

    /// A project with no remote whose recorded default branch doesn't exist locally.
    #[error("the project has no remote, and its default branch {branch} doesn't exist locally")]
    LocalDefaultMissing { branch: String },
}

impl From<BaseRefusal> for Refusal {
    fn from(refusal: BaseRefusal) -> Self {
        Refusal::new(refusal.to_string())
    }
}

impl GitError {
    /// The base-branch refusal inside this error, if it is one.
    pub fn as_base_refusal(&self) -> Option<&BaseRefusal> {
        match self {
            Self::BaseRefused(refusal) => Some(refusal),
            _ => None,
        }
    }

    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

fn exit_suffix(code: Option<i32>) -> String {
    match code {
        Some(code) => format!(" (exit code {code})"),
        None => " (killed by a signal)".to_owned(),
    }
}

fn delivery_label(delivery: DeliveryMode) -> &'static str {
    match delivery {
        DeliveryMode::DirectPr => "direct pull request",
        DeliveryMode::BranchOnly => "branch only",
        DeliveryMode::BuildInPlace => "build in place",
    }
}

fn it_or_them(settings: &[RepoTransportSetting]) -> &'static str {
    if settings.len() == 1 { "it" } else { "them" }
}

fn detail_or<'a>(detail: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = detail.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}
