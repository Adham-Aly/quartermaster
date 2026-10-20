//! `schedule` and `cancel_schedule`: scheduled wakes for the Quartermaster and secondmates.
//!
//! The handler checks what it can without the core: the time is in the future on the app's clock
//! and the note is a sensible length. The core stores the schedule in UTC with the user's time
//! zone, arms it, and posts a `schedule_due` wake to the caller when it comes due.

use crate::defs::{
    Ack, CancelScheduleArgs, CancelScheduleTool, ScheduleArgs, ScheduleTool, Scheduled,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_types::BoxFuture;
use qm_types::error::Refusal;

/// The longest note a schedule keeps, after trimming.
pub const MAX_SCHEDULE_CHARS: usize = 500;

impl Handle for ScheduleTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ScheduleArgs,
    ) -> BoxFuture<'a, Result<Scheduled, ToolFailure>> {
        Box::pin(async move {
            let now = cx.clock.now();
            if args.due <= now {
                return Err(Refusal::new(format!(
                    "the time must be in the future (it is now {now}, and {} has passed)",
                    args.due
                ))
                .into());
            }
            let what = args.what.trim();
            if what.is_empty() {
                return Err(Refusal::new("say what the schedule is for").into());
            }
            let n = what.chars().count();
            if n > MAX_SCHEDULE_CHARS {
                return Err(Refusal::new(format!(
                    "what a schedule is for is at most {MAX_SCHEDULE_CHARS} characters (this is {n})"
                ))
                .into());
            }
            cx.host
                .add_schedule(cx.caller, args.due, what.to_owned())
                .await
        })
    }
}

impl Handle for CancelScheduleTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CancelScheduleArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            cx.host.cancel_schedule(cx.caller, args.schedule).await?;
            Ok(Ack {
                note: format!("Cancelled {}.", args.schedule),
            })
        })
    }
}
