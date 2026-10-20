//! The secondmate tools: `start_secondmate`, `route_to_secondmate`, `configure_secondmate` (the
//! Quartermaster's) and `start_crewmate.secondmate` (a secondmate's).
//!
//! Each handler checks the shape of its arguments and every override it carries against the turn
//! that made the call, then calls one host method; the host (`qm_core::secondmates`) checks the
//! facts: the projects, the name, whose secondmate it is, the correlation, the scope.

use super::conversation::started_by;
use super::crew::{MAX_INSTRUCTION_CHARS, MAX_INSTRUCTIONS, check_overrides, check_start_shape};
use crate::defs::{
    Ack, ConfigureSecondmateArgs, ConfigureSecondmateTool, Held, RouteToSecondmateArgs,
    RouteToSecondmateTool, Routed, SecondmateStartCrewmateArgs, SecondmateStartCrewmateTool,
    StartSecondmateArgs, StartSecondmateTool, TaskStarted,
};
use crate::handler::{
    CharterChange, Handle, RouteRequest, SecondmateProposal, StartRequest, ToolCx, ToolFailure,
};
use qm_policy::base_branch::check_standing_branch;
use qm_policy::names::{check_branch_name, check_display_name};
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::BoxFuture;
use qm_types::error::Refusal;

/// The longest scope, in characters (the charter's `SCOPE_CHARS`).
pub const MAX_SCOPE_CHARS: usize = 300;
/// The most projects a charter lists.
pub const MAX_PROJECTS: usize = 20;
/// The longest routed request, in characters.
pub const MAX_REQUEST_CHARS: usize = 4_000;

/// The scope words, trimmed: one line of 1 to 300 characters.
pub fn check_scope(scope: &str) -> Result<String, Refusal> {
    let scope = scope.trim();
    if scope.is_empty() {
        return Err(Refusal::new("the scope can't be empty"));
    }
    if scope.contains(['\n', '\r']) {
        return Err(Refusal::new("the scope must be one line"));
    }
    let n = scope.chars().count();
    if n > MAX_SCOPE_CHARS {
        return Err(Refusal::new(format!(
            "the scope is {n} characters; the limit is {MAX_SCOPE_CHARS}"
        )));
    }
    Ok(scope.to_owned())
}

/// The project names, trimmed: at most 20, none blank, none twice (any case).
pub fn check_projects(projects: &[String]) -> Result<Vec<String>, Refusal> {
    if projects.len() > MAX_PROJECTS {
        return Err(Refusal::new(format!(
            "a secondmate takes at most {MAX_PROJECTS} projects (this one has {})",
            projects.len()
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(projects.len());
    for name in projects {
        let name = name.trim();
        if name.is_empty() {
            return Err(Refusal::new("a project name can't be empty"));
        }
        if out.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(Refusal::new(format!("{name} is listed twice")));
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

impl Handle for StartSecondmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: StartSecondmateArgs,
    ) -> BoxFuture<'a, Result<Held, ToolFailure>> {
        Box::pin(async move {
            let name = args.name.trim().to_owned();
            check_display_name(&name)?;
            let scope = check_scope(&args.scope)?;
            let projects = check_projects(&args.projects)?;
            let base_branch = args
                .base_branch
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty());
            if let Some(branch) = &base_branch {
                check_standing_branch(Authority::Turn(started_by(cx)), Some(branch))?;
            }
            cx.host
                .propose_secondmate(
                    cx.caller,
                    SecondmateProposal {
                        name,
                        scope,
                        projects,
                        base_branch,
                    },
                )
                .await
        })
    }
}

impl Handle for RouteToSecondmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: RouteToSecondmateArgs,
    ) -> BoxFuture<'a, Result<Routed, ToolFailure>> {
        Box::pin(async move {
            let request = args.request.trim().to_owned();
            if request.is_empty() {
                return Err(Refusal::new("a routed request can't be empty").into());
            }
            let n = request.chars().count();
            if n > MAX_REQUEST_CHARS {
                return Err(Refusal::new(format!(
                    "a routed request is at most {MAX_REQUEST_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            if args.instructions.len() > MAX_INSTRUCTIONS {
                return Err(Refusal::new(format!(
                    "a request takes at most {MAX_INSTRUCTIONS} instructions (this one has {})",
                    args.instructions.len()
                ))
                .into());
            }
            let mut instructions = Vec::with_capacity(args.instructions.len());
            for instruction in &args.instructions {
                let text = instruction.trim();
                let n = text.chars().count();
                if n == 0 {
                    return Err(Refusal::new("an instruction can't be empty").into());
                }
                if n > MAX_INSTRUCTION_CHARS {
                    return Err(Refusal::new(format!(
                        "an instruction is at most {MAX_INSTRUCTION_CHARS} characters (one is {n})"
                    ))
                    .into());
                }
                instructions.push(text.to_owned());
            }
            let base_branch = args
                .base_branch
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty());
            let authority = Authority::Turn(started_by(cx));
            let present = [
                (base_branch.is_some(), OverrideKind::BaseBranch),
                (args.model.is_some(), OverrideKind::NamedModel),
                (args.effort.is_some(), OverrideKind::Effort),
                (args.premium, OverrideKind::Premium),
                (!args.later_steps.is_empty(), OverrideKind::LaterSteps),
            ];
            for (_, kind) in present.iter().filter(|(on, _)| *on) {
                check_override(authority, *kind)?;
            }
            if let Some(branch) = &base_branch {
                check_branch_name(branch)?;
            }
            cx.host
                .route_to_secondmate(
                    cx.caller,
                    RouteRequest {
                        secondmate: args.secondmate,
                        request,
                        instructions,
                        base_branch,
                        model: args.model,
                        effort: args.effort,
                        premium: args.premium,
                        later_steps: args.later_steps,
                    },
                )
                .await
        })
    }
}

impl Handle for ConfigureSecondmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ConfigureSecondmateArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let set = args
                .standing_base_branch
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty());
            if set.is_some() && args.clear_standing_base_branch {
                return Err(Refusal::new("set the standing branch or clear it, not both").into());
            }
            if args.scope.is_none()
                && args.projects.is_none()
                && set.is_none()
                && !args.clear_standing_base_branch
            {
                return Err(Refusal::new("nothing to change").into());
            }
            let standing_branch = if args.clear_standing_base_branch {
                Some(None)
            } else {
                set.map(Some)
            };
            if let Some(branch) = &standing_branch {
                check_standing_branch(Authority::Turn(started_by(cx)), branch.as_deref())?;
            }
            let scope = args.scope.as_deref().map(check_scope).transpose()?;
            let projects = args.projects.as_deref().map(check_projects).transpose()?;
            cx.host
                .configure_secondmate(
                    cx.caller,
                    CharterChange {
                        secondmate: args.secondmate,
                        scope,
                        projects,
                        standing_branch,
                    },
                )
                .await
        })
    }
}

impl Handle for SecondmateStartCrewmateTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: SecondmateStartCrewmateArgs,
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
                base_branch: args
                    .base_branch
                    .map(|b| b.trim().to_owned())
                    .filter(|b| !b.is_empty()),
                verification: args.verification,
                stronger_retry_of: args.stronger_retry_of,
                correlation: args.correlation,
            };
            check_start_shape(&request)?;
            // A secondmate's base branch isn't an override: the app fills it in and refuses one
            // that doesn't match, so it is left out of the override check.
            let overrides = StartRequest {
                base_branch: None,
                ..request.clone()
            };
            check_overrides(cx, &overrides)?;
            cx.host.start_crewmate(cx.caller, request).await
        })
    }
}
