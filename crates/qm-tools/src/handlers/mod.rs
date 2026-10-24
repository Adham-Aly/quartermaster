//! Tool handlers, one module per group, bound in [`HANDLERS`].
//!
//! A new group is a module here and a line in [`HANDLERS`]. A registered tool without a binding
//! answers "isn't available in this build yet".

use crate::handler::{Binding, bind};

pub mod github;

pub mod verification;

pub mod crew;
pub mod crewmate;

pub mod conversation;
pub mod instructions;
pub mod projects;

pub mod secondmates;

pub mod deferred;
pub mod parent;
pub mod schedules;

pub mod plans;

/// Every bound handler, e.g. `bind::<ReplyTool>()`.
pub static HANDLERS: &[Binding] = &[
    bind::<crate::defs::ReplyTool>(),
    bind::<crate::defs::AskUserTool>(),
    bind::<crate::defs::AcknowledgeTool>(),
    bind::<crate::defs::SaveInstructionTool>(),
    bind::<crate::defs::DropInstructionTool>(),
    bind::<crate::defs::StartCrewmateTool>(),
    bind::<crate::defs::MessageTaskTool>(),
    bind::<crate::defs::StopTaskTool>(),
    bind::<crate::defs::SetTaskProjectTool>(),
    bind::<crate::defs::CloseTaskTool>(),
    bind::<crate::defs::LinkTasksTool>(),
    bind::<crate::defs::TaskStatusTool>(),
    bind::<crate::defs::FindTasksTool>(),
    bind::<crate::defs::ReportTool>(),
    bind::<crate::defs::AskDecisionTool>(),
    bind::<crate::defs::DeclareWaitTool>(),
    bind::<crate::defs::FileFollowupTool>(),
    bind::<crate::defs::DeliveryCheckTool>(),
    bind::<crate::defs::FindProjectFolderTool>(),
    bind::<crate::defs::CloneRepositoryTool>(),
    bind::<crate::defs::ConfigureProjectTool>(),
    bind::<crate::defs::StartSecondmateTool>(),
    bind::<crate::defs::RouteToSecondmateTool>(),
    bind::<crate::defs::ConfigureSecondmateTool>(),
    bind::<crate::defs::CloseSecondmateTool>(),
    bind::<crate::defs::SecondmateStartCrewmateTool>(),
    bind::<crate::defs::ParentReportTool>(),
    bind::<crate::defs::PrOpenTool>(),
    bind::<crate::defs::PrStatusTool>(),
    bind::<crate::defs::CiResultsTool>(),
    bind::<crate::defs::GithubReadTool>(),
    bind::<crate::defs::GithubWriteTool>(),
    bind::<crate::defs::SettleTaskTool>(),
    bind::<crate::defs::CiDiagnosisTool>(),
    bind::<crate::defs::CiRerunTool>(),
    bind::<crate::defs::PresentHoldTool>(),
    bind::<crate::defs::SetVerificationTool>(),
    bind::<crate::defs::ScheduleTool>(),
    bind::<crate::defs::CancelScheduleTool>(),
    bind::<crate::defs::PlanCheckInteractiveTool>(),
    bind::<crate::defs::PlanCheckInternalTool>(),
    bind::<crate::defs::PlanReportInteractiveTool>(),
    bind::<crate::defs::PlanReportInternalTool>(),
    bind::<crate::defs::AskDecisionPlanInternalTool>(),
    bind::<crate::defs::ApprovePlanTool>(),
    bind::<crate::defs::ProposeReleaseTagTool>(),
    bind::<crate::defs::ReleaseStatusTool>(),
    bind::<crate::defs::UnlinkTasksTool>(),
    bind::<crate::defs::ScheduleCrewmateTool>(),
    bind::<crate::defs::SecondmateScheduleCrewmateTool>(),
    bind::<crate::defs::CancelCrewmateStartTool>(),
    bind::<crate::defs::ListCrewmateStartsTool>(),
    bind::<crate::defs::FindUserRequestsTool>(),
    bind::<crate::defs::SetTaskLabelTool>(),
];
