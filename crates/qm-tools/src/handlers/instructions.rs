//! `save_instruction` and `drop_instruction`: your standing instructions.
//!
//! Both are overrides: accepted only on a turn your own message started, checked
//! by `qm_policy::overrides::check_override` before anything is written.

use super::conversation::started_by;
use crate::defs::{
    Ack, DropInstructionArgs, DropInstructionTool, InstructionSaved, SaveInstructionArgs,
    SaveInstructionTool,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_types::BoxFuture;
use qm_types::error::Refusal;

/// The longest standing instruction, after trimming.
pub const MAX_INSTRUCTION_CHARS: usize = 500;

impl Handle for SaveInstructionTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: SaveInstructionArgs,
    ) -> BoxFuture<'a, Result<InstructionSaved, ToolFailure>> {
        Box::pin(async move {
            let started_by = started_by(cx);
            check_override(Authority::Turn(started_by), OverrideKind::SaveInstruction)?;
            let text = args.text.trim();
            if text.is_empty() {
                return Err(Refusal::new("a standing instruction can't be empty").into());
            }
            let n = text.chars().count();
            if n > MAX_INSTRUCTION_CHARS {
                return Err(Refusal::new(format!(
                    "a standing instruction is at most {MAX_INSTRUCTION_CHARS} characters (this one is {n})"
                ))
                .into());
            }
            // `check_override` passed, so the user's message started this turn.
            let Some(your_message) = started_by.message() else {
                return Err(Refusal::new(
                    "saving a standing instruction needs a turn started by the user's own message",
                )
                .into());
            };
            let instruction = cx
                .host
                .save_instruction(cx.caller, your_message, text)
                .await?;
            Ok(InstructionSaved { instruction })
        })
    }
}

impl Handle for DropInstructionTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: DropInstructionArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            check_override(
                Authority::Turn(started_by(cx)),
                OverrideKind::DropInstruction,
            )?;
            cx.host
                .drop_instruction(cx.caller, args.instruction)
                .await?;
            Ok(Ack {
                note: format!("Dropped {}.", args.instruction),
            })
        })
    }
}
