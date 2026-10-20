//! `parent_report`: a secondmate's judgment on the parent channel.
//!
//! The handler checks the judgment's shape only; the arguments carry no facts (the schema refuses
//! any extra field), because the facts are always the app's. The host (`qm_core::parent_channel`)
//! checks the caller and the correlation, writes the row with its own facts snapshot and wakes the
//! Quartermaster.

use crate::defs::{Ack, ParentReportArgs, ParentReportTool};
use crate::handler::{Handle, ParentJudgment, ToolCx, ToolFailure};
use qm_types::BoxFuture;
use qm_types::error::Refusal;

/// The longest summary, in characters.
pub const MAX_SUMMARY_CHARS: usize = 2_000;
/// The longest recommendation or `needs_user`, in characters.
pub const MAX_NOTE_CHARS: usize = 1_000;

/// An optional note, trimmed: blank is none; at most [`MAX_NOTE_CHARS`].
fn check_note(field: &str, text: Option<String>) -> Result<Option<String>, Refusal> {
    let Some(text) = text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let n = text.chars().count();
    if n > MAX_NOTE_CHARS {
        return Err(Refusal::new(format!(
            "{field} is at most {MAX_NOTE_CHARS} characters (this one is {n})"
        )));
    }
    Ok(Some(text))
}

impl Handle for ParentReportTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ParentReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let summary = args.summary.trim().to_owned();
            if summary.is_empty() {
                return Err(Refusal::new("the summary can't be empty").into());
            }
            let n = summary.chars().count();
            if n > MAX_SUMMARY_CHARS {
                return Err(Refusal::new(format!(
                    "the summary is at most {MAX_SUMMARY_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            let recommendation = check_note("recommendation", args.recommendation)?;
            let needs_user = check_note("needs_user", args.needs_user)?;
            cx.host
                .parent_report(
                    cx.caller,
                    ParentJudgment {
                        correlation: args.correlation,
                        summary,
                        recommendation,
                        needs_user,
                    },
                )
                .await
        })
    }
}
