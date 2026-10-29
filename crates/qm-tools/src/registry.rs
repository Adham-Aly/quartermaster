//! The one registry of tool definitions.
//!
//! Add new tools at the end; never reorder. Every tool is listed once per variant; [`for_role`]
//! picks what one agent is offered on one engine.

use crate::defs;
use crate::spec::ToolDef;
use qm_engine::ToolDescriptor;
use qm_types::domain::{EngineKind, PlanMode, Role};

/// Every tool definition.
pub static TOOLS: &[ToolDef] = &[
    // Quartermaster and secondmates: talking to you.
    ToolDef::of::<defs::ReplyTool>(),
    ToolDef::of::<defs::AskUserTool>(),
    ToolDef::of::<defs::AcknowledgeTool>(),
    // Starting and running work.
    ToolDef::of::<defs::StartCrewmateTool>(),
    ToolDef::of::<defs::SecondmateStartCrewmateTool>(),
    ToolDef::of::<defs::MessageTaskTool>(),
    ToolDef::of::<defs::StopTaskTool>(),
    ToolDef::of::<defs::SetTaskProjectTool>(),
    ToolDef::of::<defs::CloseTaskTool>(),
    ToolDef::of::<defs::SettleTaskTool>(),
    ToolDef::of::<defs::PresentHoldTool>(),
    ToolDef::of::<defs::LinkTasksTool>(),
    ToolDef::of::<defs::TaskStatusTool>(),
    ToolDef::of::<defs::FindTasksTool>(),
    ToolDef::of::<defs::SetVerificationTool>(),
    // Secondmates.
    ToolDef::of::<defs::StartSecondmateTool>(),
    ToolDef::of::<defs::RouteToSecondmateTool>(),
    ToolDef::of::<defs::ConfigureSecondmateTool>(),
    ToolDef::of::<defs::CloseSecondmateTool>(),
    ToolDef::of::<defs::ParentReportTool>(),
    // Projects.
    ToolDef::of::<defs::FindProjectFolderTool>(),
    ToolDef::of::<defs::CloneRepositoryTool>(),
    ToolDef::of::<defs::ConfigureProjectTool>(),
    // Standing instructions and schedules.
    ToolDef::of::<defs::SaveInstructionTool>(),
    ToolDef::of::<defs::DropInstructionTool>(),
    ToolDef::of::<defs::ScheduleTool>(),
    ToolDef::of::<defs::CancelScheduleTool>(),
    // Crewmates.
    ToolDef::of::<defs::ReportTool>(),
    ToolDef::of::<defs::AskDecisionTool>(),
    ToolDef::of::<defs::DeclareWaitTool>(),
    ToolDef::of::<defs::FileFollowupTool>(),
    ToolDef::of::<defs::DeliveryCheckTool>(),
    // Crewmates: GitHub and CI.
    ToolDef::of::<defs::PrOpenTool>(),
    ToolDef::of::<defs::PrStatusTool>(),
    ToolDef::of::<defs::CiResultsTool>(),
    ToolDef::of::<defs::CiDiagnosisTool>(),
    ToolDef::of::<defs::CiRerunTool>(),
    ToolDef::of::<defs::GithubReadTool>(),
    ToolDef::of::<defs::GithubWriteTool>(),
    // Crewmates on the provider engine: files and shell.
    ToolDef::of::<defs::ReadTool>(),
    ToolDef::of::<defs::WriteTool>(),
    ToolDef::of::<defs::EditTool>(),
    ToolDef::of::<defs::GrepTool>(),
    ToolDef::of::<defs::FindTool>(),
    ToolDef::of::<defs::LsTool>(),
    ToolDef::of::<defs::BashTool>(),
    ToolDef::of::<defs::JobOutputTool>(),
    ToolDef::of::<defs::JobKillTool>(),
    // Crewmates on the provider engine: helper agents.
    ToolDef::of::<defs::AgentTool>(),
    // Plans: the planner's mode variants, and the user's approval.
    ToolDef::of::<defs::PlanCheckInteractiveTool>(),
    ToolDef::of::<defs::PlanCheckInternalTool>(),
    ToolDef::of::<defs::PlanReportInteractiveTool>(),
    ToolDef::of::<defs::PlanReportInternalTool>(),
    ToolDef::of::<defs::AskDecisionPlanInternalTool>(),
    ToolDef::of::<defs::ApprovePlanTool>(),
    // Crewmates: release tags, created and pushed by the app after the user's approval.
    ToolDef::of::<defs::ProposeReleaseTagTool>(),
    // Supervisors: what GitHub shows after a release tag is published.
    ToolDef::of::<defs::ReleaseStatusTool>(),
    // Supervisors: removing obsolete task dependencies.
    ToolDef::of::<defs::UnlinkTasksTool>(),
    ToolDef::of::<defs::ScheduleCrewmateTool>(),
    ToolDef::of::<defs::SecondmateScheduleCrewmateTool>(),
    ToolDef::of::<defs::CancelCrewmateStartTool>(),
    ToolDef::of::<defs::ListCrewmateStartsTool>(),
    ToolDef::of::<defs::FindUserRequestsTool>(),
    // Supervisors: moving a crewmate to another task.
    ToolDef::of::<defs::SetTaskLabelTool>(),
    // Supervisors: the user's go-ahead on a review's findings (normal autonomy).
    ToolDef::of::<defs::ApproveReviewTool>(),
];

/// The tools one role is offered on one engine, in registry order. `plan` is the plan mode a plan
/// task's crewmate's session was started in; `None` for every other caller.
pub fn for_role(
    role: Role,
    engine: EngineKind,
    plan: Option<PlanMode>,
) -> impl Iterator<Item = &'static ToolDef> {
    TOOLS.iter().filter(move |d| d.serves(role, engine, plan))
}

/// The tool a caller calls by this name: at most one variant of a name is offered to a caller.
pub fn find(role: Role, plan: Option<PlanMode>, name: &str) -> Option<&'static ToolDef> {
    TOOLS
        .iter()
        .find(|d| d.name == name && d.offered(role, plan))
}

/// What goes in a session spec's `tools`: the role's tools whose calls come back to the core, the
/// same on every engine. The provider engine's file and shell tools are not among them: they run
/// inside that engine, which offers them itself when the spec asks for
/// `Builtins::ProviderFileTools`.
pub fn descriptors(role: Role, plan: Option<PlanMode>) -> Vec<ToolDescriptor> {
    TOOLS
        .iter()
        .filter(|d| d.offered(role, plan) && d.scope == crate::EngineScope::Both)
        .map(ToolDef::descriptor)
        .collect()
}
