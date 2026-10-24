//! Plan tools: a plan task's crewmate's `plan_check`, its `report` and `ask_decision` variants
//! (one per plan mode, picked by the app from the project's autonomy), and the orchestrators'
//! `approve_plan`.

use super::{Ack, AskDecisionArgs};
use qm_types::domain::ReportOutcome;
use qm_types::ids::TaskId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Checks technical `plan.md` and planner-authored `plan.html`, adding review controls.
    PlanCheckInteractiveTool {
        name: "plan_check",
        key: "plan_check.interactive",
        plan: Only(Interactive),
        class: Read,
        roles: [Crewmate],
        args: PlanCheckArgs,
        output: PlanChecked,
    }
}

tool! {
    /// Checks `plan.md` under auto autonomy.
    PlanCheckInternalTool {
        name: "plan_check",
        key: "plan_check.internal",
        plan: Only(Internal),
        class: Read,
        roles: [Crewmate],
        args: PlanCheckArgs,
        output: PlanChecked,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanCheckArgs {}

/// What `plan_check` found: one line when the plan is fine, else every problem by line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChecked {
    pub result: String,
}

tool! {
    /// A plan round's report under normal autonomy: done or blocked, never a decision.
    PlanReportInteractiveTool {
        name: "report",
        key: "report.plan_interactive",
        plan: Only(Interactive),
        class: Act,
        roles: [Crewmate],
        args: PlanInteractiveReportArgs,
        output: Ack,
    }
}

// There is no third outcome on purpose: a planner's questions under normal autonomy reach the
// user only through the plan's page. (A doc comment here would reach the model's schema.)
/// A round's outcome: done or blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanRoundOutcome {
    Done,
    Blocked,
}

impl From<PlanRoundOutcome> for ReportOutcome {
    fn from(outcome: PlanRoundOutcome) -> Self {
        match outcome {
            PlanRoundOutcome::Done => Self::Done,
            PlanRoundOutcome::Blocked => Self::Blocked,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanInteractiveReportArgs {
    pub outcome: PlanRoundOutcome,
    // A short summary of the round, never the plan itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

tool! {
    /// A plan's report under auto autonomy.
    PlanReportInternalTool {
        name: "report",
        key: "report.plan_internal",
        plan: Only(Internal),
        class: Act,
        roles: [Crewmate],
        args: PlanInternalReportArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanInternalReportArgs {
    pub outcome: ReportOutcome,
    // A short summary of the plan, never the plan itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

tool! {
    /// `ask_decision` for a plan under auto autonomy: only for what truly blocks.
    AskDecisionPlanInternalTool {
        name: "ask_decision",
        key: "ask_decision.plan_internal",
        plan: Only(Internal),
        class: Act,
        roles: [Crewmate],
        args: AskDecisionArgs,
        output: Ack,
    }
}

tool! {
    /// Records that the user approved a plan; only on a turn the user's own message started.
    ApprovePlanTool {
        name: "approve_plan",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: ApprovePlanArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovePlanArgs {
    pub task: TaskId,
    // The user approved with the planner's recommended answer for every open question.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub take_recommended: bool,
}
