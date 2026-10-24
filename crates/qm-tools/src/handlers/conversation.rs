//! `reply`, `ask_user`, `acknowledge` and read-only `find_user_requests`.
//!
//! Checks run before anything is written; refusals are plain sentences the model can act on.
//! Speaking tools count as effective actions; looking up original sources changes nothing.

use crate::defs::{
    AcknowledgeArgs, AcknowledgeTool, AskUserArgs, AskUserTool, MessageShown, ReplyArgs, ReplyTool,
};
use crate::handler::{Handle, NewMessage, ToolCx, ToolFailure};
use qm_policy::action_rule::{check_acknowledgement, check_reply};
use qm_types::BoxFuture;
use qm_types::domain::{MessageKind, StartedBy};
use qm_types::error::Refusal;

/// The most one-click answers a question card offers (keys 1–9).
pub const MAX_OPTIONS: usize = 9;
/// The longest one-click answer, after trimming.
pub const MAX_OPTION_CHARS: usize = 60;

/// Who started the caller's turn; a caller with no turn is treated as an app-event turn.
pub(crate) fn started_by(cx: &ToolCx<'_>) -> StartedBy {
    cx.caller
        .turn
        .map_or(StartedBy::AppEvent, |turn| turn.started_by)
}

impl Handle for ReplyTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ReplyArgs,
    ) -> BoxFuture<'a, Result<MessageShown, ToolFailure>> {
        Box::pin(async move {
            check_reply(&args.text)?;
            let message = cx
                .host
                .show_message(
                    cx.caller,
                    NewMessage {
                        kind: MessageKind::Reply,
                        text: args.text,
                        options: Vec::new(),
                    },
                )
                .await?;
            Ok(MessageShown { message })
        })
    }
}

/// The options as they will be shown: trimmed, each checked.
fn check_options(options: Vec<String>) -> Result<Vec<String>, Refusal> {
    if options.len() > MAX_OPTIONS {
        return Err(Refusal::new(format!(
            "a question has at most {MAX_OPTIONS} options (this one has {})",
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
        if n > MAX_OPTION_CHARS {
            return Err(Refusal::new(format!(
                "an option is at most {MAX_OPTION_CHARS} characters (\"{option}\" is {n})"
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

impl Handle for AskUserTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: AskUserArgs,
    ) -> BoxFuture<'a, Result<MessageShown, ToolFailure>> {
        Box::pin(async move {
            if args.question.trim().is_empty() {
                return Err(Refusal::new("a question can't be empty").into());
            }
            let options = check_options(args.options)?;
            let message = cx
                .host
                .show_message(
                    cx.caller,
                    NewMessage {
                        kind: MessageKind::Question,
                        text: args.question,
                        options,
                    },
                )
                .await?;
            Ok(MessageShown { message })
        })
    }
}

/// Why `acknowledge` is refused on a turn no message of yours started.
pub const ACKNOWLEDGE_NEEDS_MESSAGE: &str = "acknowledge sits beside the user's message, and no message of theirs started this turn; use reply";

impl Handle for AcknowledgeTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: AcknowledgeArgs,
    ) -> BoxFuture<'a, Result<MessageShown, ToolFailure>> {
        Box::pin(async move {
            check_acknowledgement(&args.text)?;
            let Some(your_message) = started_by(cx).message() else {
                return Err(Refusal::new(ACKNOWLEDGE_NEEDS_MESSAGE).into());
            };
            let message = cx
                .host
                .acknowledge(cx.caller, your_message, args.text.trim())
                .await?;
            Ok(MessageShown { message })
        })
    }
}

impl Handle for crate::defs::FindUserRequestsTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: crate::defs::FindUserRequestsArgs,
    ) -> BoxFuture<'a, Result<crate::defs::FoundUserRequests, ToolFailure>> {
        Box::pin(async move { cx.host.find_user_requests(cx.caller, args).await })
    }
}
