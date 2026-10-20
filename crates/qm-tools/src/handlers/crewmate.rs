//! A crewmate's tools: `report`, `ask_decision`, `declare_wait`, `file_followup` and
//! `delivery_check`.
//!
//! Each handler checks shapes, then calls one host method; the host checks the facts against the
//! caller's task (the headings for its level, the commits, the waited-on task, the project).

use crate::defs::{
    Ack, AskDecisionArgs, AskDecisionTool, DeclareWaitArgs, DeclareWaitTool, DeliveryCheck,
    DeliveryCheckArgs, DeliveryCheckTool, FileFollowupArgs, FileFollowupTool, ReportArgs,
    ReportTool,
};
use crate::handler::{DeclaredWait, Handle, ToolCx, ToolFailure};
use qm_policy::names::check_task_title;
use qm_types::BoxFuture;
use qm_types::error::Refusal;

/// The most options a decision question offers (matches `descriptions/ask_decision.md`).
pub const MAX_DECISION_OPTIONS: usize = 9;
/// The longest option, in characters after trimming.
pub const MAX_DECISION_OPTION_CHARS: usize = 60;
/// The longest decision question, in characters.
pub const MAX_QUESTION_CHARS: usize = 2_000;
/// The longest description of an outside wait, in characters.
pub const MAX_EXTERNAL_CHARS: usize = 500;
/// The longest follow-up goal, in characters.
pub const MAX_FOLLOWUP_GOAL_CHARS: usize = 4_000;

impl Handle for ReportTool {
    fn handle<'a>(cx: &'a ToolCx<'a>, args: ReportArgs) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move { cx.host.record_report(cx.caller, args).await })
    }
}

/// The options as they will be shown: trimmed, each checked.
pub fn check_decision_options(options: Vec<String>) -> Result<Vec<String>, Refusal> {
    if options.len() > MAX_DECISION_OPTIONS {
        return Err(Refusal::new(format!(
            "a decision offers at most {MAX_DECISION_OPTIONS} options (this one has {})",
            options.len()
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(options.len());
    for option in options {
        let option = option.trim().to_owned();
        let n = option.chars().count();
        if n == 0 {
            return Err(Refusal::new("an option can't be empty"));
        }
        if n > MAX_DECISION_OPTION_CHARS {
            return Err(Refusal::new(format!(
                "an option is at most {MAX_DECISION_OPTION_CHARS} characters (\"{option}\" is {n})"
            )));
        }
        if out.iter().any(|o| o.eq_ignore_ascii_case(&option)) {
            return Err(Refusal::new(format!(
                "the option \"{option}\" appears twice; each option must be different"
            )));
        }
        out.push(option);
    }
    Ok(out)
}

impl Handle for AskDecisionTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: AskDecisionArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let question = args.question.trim();
            if question.is_empty() {
                return Err(Refusal::new("a decision question can't be empty").into());
            }
            let n = question.chars().count();
            if n > MAX_QUESTION_CHARS {
                return Err(Refusal::new(format!(
                    "a decision question is at most {MAX_QUESTION_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            let options = check_decision_options(args.options)?;
            cx.host
                .ask_decision(cx.caller, question.to_owned(), options)
                .await
        })
    }
}

impl Handle for DeclareWaitTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: DeclareWaitArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let external = args
                .external
                .map(|e| e.trim().to_owned())
                .filter(|e| !e.is_empty());
            let wait = match (args.task, external) {
                (Some(task), None) => DeclaredWait::Task(task),
                (None, Some(external)) => {
                    let n = external.chars().count();
                    if n > MAX_EXTERNAL_CHARS {
                        return Err(Refusal::new(format!(
                            "describe the outside wait in at most {MAX_EXTERNAL_CHARS} characters (this is {n})"
                        ))
                        .into());
                    }
                    DeclaredWait::External(external)
                }
                (Some(_), Some(_)) => {
                    return Err(Refusal::new(
                        "name either the task you wait on or the outside thing, not both",
                    )
                    .into());
                }
                (None, None) => {
                    return Err(Refusal::new(
                        "name the task you wait on (task) or the outside thing (external)",
                    )
                    .into());
                }
            };
            cx.host.declare_wait(cx.caller, wait).await
        })
    }
}

impl Handle for FileFollowupTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: FileFollowupArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            check_task_title(args.title.trim())?;
            let goal = args.goal.trim();
            if goal.is_empty() {
                return Err(Refusal::new("a follow-up needs a goal").into());
            }
            let n = goal.chars().count();
            if n > MAX_FOLLOWUP_GOAL_CHARS {
                return Err(Refusal::new(format!(
                    "a follow-up's goal is at most {MAX_FOLLOWUP_GOAL_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            let followup = FileFollowupArgs {
                title: args.title.trim().to_owned(),
                goal: goal.to_owned(),
                kind: args.kind,
                project: args
                    .project
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty()),
            };
            cx.host.file_followup(cx.caller, followup).await
        })
    }
}

impl Handle for DeliveryCheckTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: DeliveryCheckArgs,
    ) -> BoxFuture<'a, Result<DeliveryCheck, ToolFailure>> {
        let DeliveryCheckArgs {} = args;
        Box::pin(async move { cx.host.delivery_check(cx.caller).await })
    }
}
