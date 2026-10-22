//! A crewmate's tools for reporting, asking, waiting and checking delivery.

use super::Ack;
use qm_types::domain::{
    FindingAction, ReportOutcome, ReviewVerdict, RiskLevel, Severity, TaskKind,
};
use qm_types::ids::TaskId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Reports done, blocked or needs a decision, and wakes the supervisor. The app checks the
    /// headings for the task's verification level, the answer for answer-only kinds, `reproduced`
    /// for reproductions, the verdict, findings and risk for reviews, and on "done" the commit
    /// names.
    ReportTool {
        name: "report",
        key: "report",
        plan: NotPlan,
        class: Act,
        roles: [Crewmate],
        args: ReportArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportArgs {
    pub outcome: ReportOutcome,
    // The five headings: Intent · What changed · Verified · Risk · Not done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what_changed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_done: Option<String>,
    // Answer-only kinds: the answer the task's report carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    // Reproductions: the report opens with "reproduced: yes|no".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduced: Option<bool>,
    // A research answer marked as a confirmed diagnosis a fix can build on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub diagnosis: bool,
    // Review tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewReport>,
    // Gaps the crewmate couldn't close, e.g. from delivery_check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

// A review's verdict, findings and risk rating.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewReport {
    pub verdict: ReviewVerdict,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    pub risk: RiskLevel,
    pub risk_reason: String,
}

// One review finding: severity, file and line, one sentence, and the action it asks for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: Severity,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub summary: String,
    pub action: FindingAction,
}

tool! {
    /// Asks the supervisor for a decision; wakes the supervisor, never you directly; the
    /// watchdogs pause while it waits.
    AskDecisionTool {
        name: "ask_decision",
        key: "ask_decision",
        plan: NotPlan,
        class: Act,
        roles: [Crewmate],
        args: AskDecisionArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskDecisionArgs {
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

tool! {
    /// Declares a wait on another task or something outside; names the task or the thing; the
    /// watchdogs pause; a waited-on task finishing wakes it.
    DeclareWaitTool {
        name: "declare_wait",
        class: Act,
        roles: [Crewmate],
        args: DeclareWaitArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareWaitArgs {
    // Exactly one of the two (checked by the handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
}

tool! {
    /// Files a follow-up task; it lands in the supervisor's queue as a proposal, not a running
    /// task.
    FileFollowupTool {
        name: "file_followup",
        class: Act,
        roles: [Crewmate],
        args: FileFollowupArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileFollowupArgs {
    pub title: String,
    pub goal: String,
    pub kind: TaskKind,
    // Defaults to the crewmate's own project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

tool! {
    /// Before the first edit: can it push, open the pull request and read checks? The app answers
    /// from its own checks.
    DeliveryCheckTool {
        name: "delivery_check",
        class: Read,
        roles: [Crewmate],
        args: DeliveryCheckArgs,
        output: DeliveryCheck,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryCheckArgs {}

/// What the crewmate can do to deliver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryCheck {
    pub push: Capability,
    pub pull_request: Capability,
    pub checks: Capability,
    /// Gaps the app can't close, in plain sentences, to report as blockers.
    pub gaps: Vec<String>,
}

/// One delivery capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Ok,
    Missing,
    /// Not needed under this delivery mode.
    NotNeeded,
}
