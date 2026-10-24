//! Task starts (immediate and deferred), steering, linking/unlinking, and task lookups.
//!
//! Each handler checks the shape of its arguments and every override it carries, then calls one
//! host method; the host (`qm_core::tasks::TaskService`) checks the facts: the project, direct
//! reports only, the task's status, cycles.

use super::conversation::started_by;
use crate::defs::{
    CloseTaskArgs, CloseTaskTool, FindTasksArgs, FindTasksTool, FoundTasks, LinkTasksArgs,
    LinkTasksTool, Linked, MessageTaskArgs, MessageTaskTool, SetTaskProjectArgs,
    SetTaskProjectTool, StartCrewmateArgs, StartCrewmateTool, StopTaskArgs, StopTaskTool,
    TaskDetail, TaskStarted, TaskStatusArgs, TaskStatusTool, TaskUpdated, UnlinkTasksArgs,
    UnlinkTasksTool, Unlinked,
};
use crate::handler::{Handle, StartRequest, ToolCx, ToolFailure};
use qm_policy::names::check_task_title;
use qm_policy::overrides::{Authority, OverrideKind, check_choice_override};
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
/// The longest message to a task that passes on a user's plan reply (one opens with
/// `<plan-reply `), in characters: a reply can be up to 32,000 on its own.
pub const MAX_PLAN_REPLY_MESSAGE_CHARS: usize = 40_000;

/// Checks turn authority, or a model-choice citation whose user source the host must verify.
pub(crate) fn check_overrides(cx: &ToolCx<'_>, request: &StartRequest) -> Result<(), Refusal> {
    let authority = Authority::Turn(started_by(cx));
    let present = [
        (request.model.is_some(), OverrideKind::NamedModel),
        (request.tier.is_some(), OverrideKind::Tier),
        (request.effort.is_some(), OverrideKind::Effort),
        (request.premium, OverrideKind::Premium),
        (!request.later_steps.is_empty(), OverrideKind::LaterSteps),
        (request.base_branch.is_some(), OverrideKind::BaseBranch),
        (request.in_place, OverrideKind::InPlace),
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
        check_choice_override(authority, request.choice_message, *kind)?;
    }
    Ok(())
}

/// The shape of a start request, before any override or fact is checked.
pub fn check_start_shape(request: &StartRequest) -> Result<(), Refusal> {
    check_task_title(&request.title)?;
    if request.folder.is_some() && request.project.is_some() {
        return Err(Refusal::new("name a folder or a listed project, not both"));
    }
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

/// Normalises and checks both immediate and deferred Quartermaster starts.
pub(crate) fn prepare_start(
    cx: &ToolCx<'_>,
    args: StartCrewmateArgs,
) -> Result<StartRequest, Refusal> {
    let request = StartRequest {
        folder: args
            .folder
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty()),
        project: args
            .project
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty()),
        in_place: args.in_place,
        kind: args.kind,
        title: args.title.trim().to_owned(),
        goal: args.goal,
        constraints: args.constraints,
        tests_required: args.tests_required,
        builds_on: args.builds_on,
        instructions: args.instructions,
        later_steps: args.later_steps,
        model: args.model,
        preference: args.preference,
        tier: args.tier,
        effort: args.effort,
        premium: args.premium,
        choice_message: args.choice_message,
        base_branch: args.base_branch,
        verification: args.verification,
        stronger_retry_of: args.stronger_retry_of,
        correlation: None,
    };
    check_start_shape(&request)?;
    check_overrides(cx, &request)?;
    Ok(request)
}

impl Handle for StartCrewmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: StartCrewmateArgs,
    ) -> BoxFuture<'a, Result<TaskStarted, ToolFailure>> {
        Box::pin(async move {
            let request = prepare_start(cx, args)?;
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
            let max = if text.contains("<plan-reply ") {
                MAX_PLAN_REPLY_MESSAGE_CHARS
            } else {
                MAX_MESSAGE_CHARS
            };
            if n > max {
                return Err(Refusal::new(format!(
                    "a message to a task is at most {max} characters (this one is {n})"
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

impl Handle for SetTaskProjectTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: SetTaskProjectArgs,
    ) -> BoxFuture<'a, Result<TaskUpdated, ToolFailure>> {
        Box::pin(async move {
            let trim = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
            let (project, folder) = (trim(args.project), trim(args.folder));
            if project.is_some() == folder.is_some() {
                return Err(Refusal::new("name a folder or a listed project: exactly one").into());
            }
            cx.host
                .set_task_project(cx.caller, args.task, project, folder)
                .await
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

impl Handle for UnlinkTasksTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: UnlinkTasksArgs,
    ) -> BoxFuture<'a, Result<Unlinked, ToolFailure>> {
        Box::pin(async move {
            if args.blocker == args.blocked {
                return Err(Refusal::new(
                    "unlink_tasks needs two different tasks; name the blocker and the task it blocks",
                )
                .into());
            }
            cx.host
                .unlink_tasks(cx.caller, args.blocker, args.blocked)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{Caller, ToolHost};
    use qm_types::clock::SystemClock;
    use qm_types::domain::{Role, TaskKind};
    use qm_types::ids::{AgentId, MessageId};

    struct Host;
    impl ToolHost for Host {}

    #[test]
    fn earlier_user_citations_pass_choice_preflight_but_not_unrelated_overrides() {
        let caller = Caller {
            agent: AgentId::new(1),
            role: Role::Quartermaster,
            turn: None,
            task: None,
            secondmate: None,
            plan: None,
        };
        let cx = ToolCx {
            caller: &caller,
            clock: &SystemClock,
            host: &Host,
        };
        let mut request = StartRequest {
            folder: None,
            project: None,
            in_place: false,
            kind: TaskKind::Research,
            title: "research(x): inspect".into(),
            goal: "inspect".into(),
            constraints: vec![],
            tests_required: false,
            builds_on: None,
            instructions: vec![],
            later_steps: vec![],
            model: None,
            preference: None,
            tier: None,
            effort: Some(qm_types::domain::Effort::High),
            premium: true,
            choice_message: None,
            base_branch: None,
            verification: None,
            stronger_retry_of: None,
            correlation: None,
        };
        assert!(check_overrides(&cx, &request).is_err());
        request.choice_message = Some(MessageId::new(7));
        assert!(
            check_overrides(&cx, &request).is_ok(),
            "the host verifies this citation"
        );
        request.base_branch = Some("main".into());
        assert!(check_overrides(&cx, &request).is_err());
        request.base_branch = None;
        request.in_place = true;
        assert!(check_overrides(&cx, &request).is_err());
    }
}
