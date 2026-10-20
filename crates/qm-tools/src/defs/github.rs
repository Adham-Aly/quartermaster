//! A crewmate's GitHub and CI tools.

use super::{Ack, Held};
use qm_types::domain::{CiDiagnosis, GithubReadKind, GithubWriteAction};
use qm_types::ids::{CommitSha, HeldActionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Pushes the branch and opens or updates a pull request. Direct-PR mode only; never for
    /// answer-only tasks; refused under `reviewed` until the review passes, and when pushing a fix
    /// without a declared diagnosis; names checked.
    PrOpenTool {
        name: "pr_open",
        class: Act,
        roles: [Crewmate],
        args: PrOpenArgs,
        output: PrOpened,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrOpenArgs {
    // type(scope): summary, lowercase imperative, at most 72 characters.
    pub title: String,
    pub body: String,
}

/// The pull request was opened or updated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrOpened {
    pub pr: u64,
    pub url: String,
    pub head: CommitSha,
}

tool! {
    /// Reads its own pull request. Read only.
    PrStatusTool {
        name: "pr_status",
        class: Read,
        roles: [Crewmate],
        args: PrStatusArgs,
        output: PrStatus,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrStatusArgs {}

/// The crewmate's pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub pr: u64,
    pub url: String,
    pub state: PrState,
    pub head: CommitSha,
    /// `None` while GitHub is still computing it.
    pub mergeable: Option<bool>,
}

/// A pull request's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

tool! {
    /// Reads its check results and logs. Read only.
    CiResultsTool {
        name: "ci_results",
        class: Read,
        roles: [Crewmate],
        args: CiResultsArgs,
        output: CiReport,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiResultsArgs {
    // A check's name, for its log excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// The checks on a head commit (every check run and commit status counts).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiReport {
    pub head: CommitSha,
    pub checks: Vec<CheckResult>,
}

/// One check run or commit status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
}

/// Where a check stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    Passed,
    Failed,
    Cancelled,
    Skipped,
}

tool! {
    /// Declares what a failing check is: own regression, suspected flake or already failing on
    /// main. Free to call; starts the repair count for a regression.
    CiDiagnosisTool {
        name: "ci_diagnosis",
        class: Act,
        roles: [Crewmate],
        args: CiDiagnosisArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiDiagnosisArgs {
    pub check: String,
    pub diagnosis: CiDiagnosis,
    // Required for a suspected flake and for failing on main (checked by the handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

tool! {
    /// Reruns checks once for a suspected flake; once per task, and only with evidence attached.
    CiRerunTool {
        name: "ci_rerun",
        class: Act,
        roles: [Crewmate],
        args: CiRerunArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CiRerunArgs {
    pub check: String,
}

tool! {
    /// Reads issues, pull requests and files from GitHub. Read only.
    GithubReadTool {
        name: "github_read",
        class: Read,
        roles: [Crewmate],
        args: GithubReadArgs,
        output: GithubContent,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubReadArgs {
    pub what: GithubReadKind,
    // owner/name; defaults to the project's repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

/// What GitHub returned, as text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubContent {
    pub content: String,
}

tool! {
    /// Comments, labels and issues. Outside its own project, or in an answer-only task, a held
    /// action.
    GithubWriteTool {
        name: "github_write",
        class: Act,
        roles: [Crewmate],
        args: GithubWriteArgs,
        output: GithubWriteOutcome,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubWriteArgs {
    pub action: GithubWriteAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

/// What `github_write` did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GithubWriteOutcome {
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Held {
        held_action: HeldActionId,
        note: String,
    },
}

impl From<Held> for GithubWriteOutcome {
    fn from(h: Held) -> Self {
        Self::Held {
            held_action: h.held_action,
            note: h.note,
        }
    }
}
