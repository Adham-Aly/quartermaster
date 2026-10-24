//! Deferred starts share normal start preparation; the host owns persistence and trigger facts.

use super::crew::prepare_start;
use super::secondmates::prepare_secondmate_start;
use crate::defs::{
    Ack, CancelCrewmateStartArgs, CancelCrewmateStartTool, DeferredStart, ListCrewmateStartsArgs,
    ListCrewmateStartsTool, ScheduleCrewmateArgs, ScheduleCrewmateTool,
    SecondmateScheduleCrewmateArgs, SecondmateScheduleCrewmateTool, StartTrigger,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_types::BoxFuture;
use qm_types::error::Refusal;

fn check_trigger(cx: &ToolCx<'_>, trigger: &StartTrigger) -> Result<(), Refusal> {
    if let StartTrigger::At { due } = trigger
        && *due <= cx.clock.now()
    {
        return Err(Refusal::new("choose a future time for the deferred start"));
    }
    Ok(())
}

impl Handle for ScheduleCrewmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ScheduleCrewmateArgs,
    ) -> BoxFuture<'a, Result<DeferredStart, ToolFailure>> {
        Box::pin(async move {
            let request = prepare_start(cx, args.start)?;
            check_trigger(cx, &args.trigger)?;
            cx.host
                .schedule_crewmate(cx.caller, request, args.trigger)
                .await
        })
    }
}

impl Handle for SecondmateScheduleCrewmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: SecondmateScheduleCrewmateArgs,
    ) -> BoxFuture<'a, Result<DeferredStart, ToolFailure>> {
        Box::pin(async move {
            let request = prepare_secondmate_start(cx, args.start)?;
            check_trigger(cx, &args.trigger)?;
            cx.host
                .schedule_crewmate(cx.caller, request, args.trigger)
                .await
        })
    }
}

impl Handle for CancelCrewmateStartTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CancelCrewmateStartArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            cx.host
                .cancel_crewmate_start(cx.caller, args.schedule)
                .await
        })
    }
}

impl Handle for ListCrewmateStartsTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        _args: ListCrewmateStartsArgs,
    ) -> BoxFuture<'a, Result<Vec<DeferredStart>, ToolFailure>> {
        Box::pin(async move { cx.host.list_crewmate_starts(cx.caller).await })
    }
}

#[cfg(test)]
mod tests;
