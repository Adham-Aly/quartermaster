//! The task tools: `start_crewmate`, `message_task`, `stop_task`, `close_task`, `link_tasks`,
//! `task_status` and `find_tasks`.
//!
//! Each handler checks the shape of its arguments and every override it carries, then calls one
//! host method; the host (`qm_core::tasks::TaskService`) checks the facts: the project, direct
//! reports only, the task's status, cycles.

use super::conversation::started_by;
use crate::defs::{
    CloseTaskArgs, CloseTaskTool, FindTasksArgs, FindTasksTool, FoundTasks, LinkTasksArgs,
    LinkTasksTool, Linked, MessageTaskArgs, MessageTaskTool, StartCrewmateArgs, StartCrewmateTool,
    StopTaskArgs, StopTaskTool, TaskDetail, TaskStarted, TaskStatusArgs, TaskStatusTool,
    TaskUpdated,
};
use crate::handler::{Handle, StartRequest, ToolCx, ToolFailure};
use qm_policy::names::check_task_title;
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::BoxFuture;
use qm_types::error::Refusal;

/// The longest goal, in characters.
pub const MAX_GOAL_CHARS: usize = 4_000;
/// The most one-time instructions a task carries.
pub const MAX_INSTRUCTIONS: usize = 20;
/// The longest one-time instruction, in characters.
pub const MAX_INSTRUCTION_CHARS: usize = 2_000;
/// The most constraints a task carries.
pub const MAX_CONSTRAINTS: usize = 20;
/// The longest message to a task, in characters.
pub const MAX_MESSAGE_CHARS: usize = 8_000;

/// Every override `request` carries, checked against the caller's turn. A caller without a turn is
/// treated as an app-event turn.
pub(crate) fn check_overrides(cx: &ToolCx<'_>, request: &StartRequest) -> Result<(), Refusal> {
    let authority = Authority::Turn(started_by(cx));
    let present = [
        (request.model.is_some(), OverrideKind::NamedModel),
        (request.effort.is_some(), OverrideKind::Effort),
        (request.premium, OverrideKind::Premium),
        (!request.later_steps.is_empty(), OverrideKind::LaterSteps),
        (request.base_branch.is_some(), OverrideKind::BaseBranch),
        (
            request.verification.is_some(),
            OverrideKind::VerificationLevel,
        ),
        (
            request.stronger_retry_of.is_some(),
            OverrideKind::StrongerModelRetry,
        ),
    ];
    for (_, kind) in present.iter().filter(|(on, _)| *on) {
        check_override(authority, *kind)?;
    }
    Ok(())
}

/// The shape of a start request, before any override or fact is checked.
pub fn check_start_shape(request: &StartRequest) -> Result<(), Refusal> {
    check_task_title(&request.title)?;
    let goal = request.goal.trim();
    if goal.is_empty() {
        return Err(Refusal::new("a task needs a goal"));
    }
    let n = goal.chars().count();
    if n > MAX_GOAL_CHARS {
        return Err(Refusal::new(format!(
            "a goal is at most {MAX_GOAL_CHARS} characters (this one is {n})"
        )));
    }
    if request.instructions.len() > MAX_INSTRUCTIONS {
        return Err(Refusal::new(format!(
            "a task takes at most {MAX_INSTRUCTIONS} instructions (this one has {})",
            request.instructions.len()
        )));
    }
    for instruction in &request.instructions {
        let n = instruction.trim().chars().count();
        if n == 0 {
            return Err(Refusal::new("an instruction can't be empty"));
        }
        if n > MAX_INSTRUCTION_CHARS {
            return Err(Refusal::new(format!(
                "an instruction is at most {MAX_INSTRUCTION_CHARS} characters (one is {n})"
            )));
        }
    }
    if request.constraints.len() > MAX_CONSTRAINTS {
        return Err(Refusal::new(format!(
            "a task takes at most {MAX_CONSTRAINTS} constraints (this one has {})",
            request.constraints.len()
        )));
    }
    if request.constraints.iter().any(|c| c.text.trim().is_empty()) {
        return Err(Refusal::new("a constraint can't be empty"));
    }
    if request.builds_on.is_some()
        && request.stronger_retry_of.is_some()
        && request.builds_on == request.stronger_retry_of
    {
        return Err(Refusal::new(
            "a stronger-model retry already builds on the task it retries; leave builds_on out",
        ));
    }
    Ok(())
}

impl Handle for StartCrewmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: StartCrewmateArgs,
    ) -> BoxFuture<'a, Result<TaskStarted, ToolFailure>> {
        Box::pin(async move {
            let request = StartRequest {
                project: args.project,
                kind: args.kind,
                title: args.title.trim().to_owned(),
                goal: args.goal,
                constraints: args.constraints,
                tests_required: args.tests_required,
                builds_on: args.builds_on,
                instructions: args.instructions,
                later_steps: args.later_steps,
                model: args.model,
                effort: args.effort,
                premium: args.premium,
                base_branch: args.base_branch,
                verification: args.verification,
                stronger_retry_of: args.stronger_retry_of,
                correlation: None,
            };
            check_start_shape(&request)?;
            check_overrides(cx, &request)?;
            cx.host.start_crewmate(cx.caller, request).await
        })
    }
}

impl Handle for MessageTaskTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: MessageTaskArgs,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move {
            let text = args.text.trim();
            if text.is_empty() {
                return Err(Refusal::new("a message to a task can't be empty").into());
            }
            let n = text.chars().count();
            if n > MAX_MESSAGE_CHARS {
                return Err(Refusal::new(format!(
                    "a message to a task is at most {MAX_MESSAGE_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            cx.host
                .message_task(cx.caller, args.task, text.to_owned())
                .await
        })
    }
}

impl Handle for StopTaskTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: StopTaskArgs,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move {
            let reason = args
                .reason
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty());
            cx.host.stop_task(cx.caller, args.task, reason).await
        })
    }
}

impl Handle for CloseTaskTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CloseTaskArgs,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move { cx.host.close_task(cx.caller, args.task).await })
    }
}

impl Handle for LinkTasksTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: LinkTasksArgs,
    ) -> BoxFuture<'a, Result<Linked, ToolFailure>> {
        Box::pin(async move {
            if args.blocker == args.blocked {
                return Err(Refusal::new(format!("{} can't block itself", args.blocker)).into());
            }
            cx.host
                .link_tasks(cx.caller, args.blocker, args.blocked)
                .await
        })
    }
}

impl Handle for TaskStatusTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: TaskStatusArgs,
    ) -> BoxFuture<'a, Result<TaskDetail, ToolFailure>> {
        Box::pin(async move { cx.host.task_detail(cx.caller, args.task).await })
    }
}

impl Handle for FindTasksTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: FindTasksArgs,
    ) -> BoxFuture<'a, Result<FoundTasks, ToolFailure>> {
        Box::pin(async move { cx.host.find_tasks(cx.caller, args).await })
    }
}
