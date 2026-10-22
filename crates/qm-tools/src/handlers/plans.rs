//! Plan tools: `plan_check` and the plan variants of `report` and `ask_decision` for a plan
//! task's crewmate, and `approve_plan` for orchestrators.
//!
//! The variants only shape the call; the host checks it against the plan's live mode and stage.

use super::conversation::started_by;
use crate::defs::{
    Ack, ApprovePlanArgs, ApprovePlanTool, AskDecisionArgs, AskDecisionPlanInternalTool,
    PlanCheckArgs, PlanCheckInteractiveTool, PlanCheckInternalTool, PlanChecked,
    PlanInteractiveReportArgs, PlanInternalReportArgs, PlanReportInteractiveTool,
    PlanReportInternalTool, ReportArgs,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::BoxFuture;
use qm_types::domain::ReportOutcome;

fn plan_check<'a>(cx: &'a ToolCx<'a>) -> BoxFuture<'a, Result<PlanChecked, ToolFailure>> {
    Box::pin(async move {
        let result = cx.host.plan_check(cx.caller).await?;
        Ok(PlanChecked { result })
    })
}

impl Handle for PlanCheckInteractiveTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        _args: PlanCheckArgs,
    ) -> BoxFuture<'a, Result<PlanChecked, ToolFailure>> {
        plan_check(cx)
    }
}

impl Handle for PlanCheckInternalTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        _args: PlanCheckArgs,
    ) -> BoxFuture<'a, Result<PlanChecked, ToolFailure>> {
        plan_check(cx)
    }
}

/// A plan report as the general report's shape: only an outcome, a summary and blockers.
fn as_report(outcome: ReportOutcome, answer: Option<String>, blockers: Vec<String>) -> ReportArgs {
    ReportArgs {
        outcome,
        intent: None,
        what_changed: None,
        verified: None,
        risk: None,
        not_done: None,
        answer,
        reproduced: None,
        diagnosis: false,
        review: None,
        blockers,
    }
}

impl Handle for PlanReportInteractiveTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: PlanInteractiveReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let report = as_report(args.outcome.into(), args.answer, args.blockers);
        Box::pin(async move { cx.host.record_report(cx.caller, report).await })
    }
}

impl Handle for PlanReportInternalTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: PlanInternalReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        let report = as_report(args.outcome, args.answer, args.blockers);
        Box::pin(async move { cx.host.record_report(cx.caller, report).await })
    }
}

impl Handle for AskDecisionPlanInternalTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: AskDecisionArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(super::crewmate::ask_decision(cx, args))
    }
}

impl Handle for ApprovePlanTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ApprovePlanArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            check_override(Authority::Turn(started_by(cx)), OverrideKind::PlanApproval)?;
            cx.host
                .approve_plan(cx.caller, args.task, args.take_recommended)
                .await
        })
    }
}
