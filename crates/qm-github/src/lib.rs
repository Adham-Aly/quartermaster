//! GitHub: pull requests, checks and merges.
//!
//! GitHub's API from Rust through `octocrab`, with the token from `gh auth token`. Opens and
//! updates pull requests for `pr_open`, polls a head commit's checks every 30 seconds while any are
//! pending ("passed" means every check run and every commit status on that exact commit
//! succeeded and every required check is present), and merges through the asynchronous merge API
//! with the approved SHA as a precondition, so GitHub refuses if the head moved.
//!
//! The credentials fence lives here too: crewmate processes get a logged-out `gh` and
//! environment-only git config that makes any direct push fail; only the app pushes, after its
//! checks, in its own environment.
//!
//! The crate knows GitHub and nothing of tasks: the core maps its types to the rules.
//!
//! - [`token`]: [`TokenSource`], [`GhCli`], [`GithubToken`].
//! - [`repo`]: [`RepoRef`] and remote URL parsing.
//! - [`client`]: [`GithubClient`] (octocrab per token, the rate limit).
//! - [`pulls`], [`checks`], [`logs`], [`content`]: the calls.
//! - [`fence`]: the crewmate fence's constants and their check.
//! - [`releases`]: a public repository's newest release, read without a login (the app's update
//!   check).

pub mod checks;
pub mod client;
pub mod content;
mod error;
pub mod fence;
pub mod logs;
pub mod pulls;
pub mod releases;
pub mod repo;
pub mod token;

pub use checks::{
    CheckRun, CheckSnapshot, CommitStatus, PASSING_CONCLUSIONS, RequiredChecks, RunStatus,
};
pub use client::GithubClient;
pub use content::{GithubRead, GithubWrite};
pub use error::{GithubError, RateState};
pub use pulls::{
    MergeResult, PrState, PullRequest, RepoInfo, SquashCommit, SquashRules, squash_request,
};
pub use releases::{Release, latest_release};
pub use repo::{RepoRef, redact_url};
pub use token::{GhCli, GhProgram, GithubToken, TokenSource, fixed_gh};
