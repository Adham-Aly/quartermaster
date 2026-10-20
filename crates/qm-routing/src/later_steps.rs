//! Carrying your model choices through a workflow.
//!
//! "Recorded on your turn, applied by the app later. … When you do name models or effort for
//! later steps, the Quartermaster records them on the first task as typed `later_steps`, for
//! example `{kind: build, model: sonnet-5-5, effort: medium}`, and only on your turns. When a
//! later task of that kind builds on the first task, directly or further down the chain, the app
//! applies the entry itself, whatever turn starts it. The routing record cites your message. Newer
//! words of yours, or a stronger-model retry you pick, take precedence."
//!
//! Recording needs a turn you started (`qm_policy::overrides`, `OverrideKind::LaterSteps`); this
//! module validates the entries and finds the one that applies.

use qm_types::domain::{Effort, TaskKind};
use qm_types::error::Refusal;
use qm_types::ids::{MessageId, ModelId, TaskId};
use serde::{Deserialize, Serialize};

/// One `later_steps` entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaterStep {
    pub kind: TaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

/// Checks the entries a tool call carries: each names a model or an effort, and no kind appears
/// twice (which one would apply would be ambiguous).
pub fn check_later_steps(steps: &[LaterStep]) -> Result<(), Refusal> {
    for (i, step) in steps.iter().enumerate() {
        if step.model.is_none() && step.effort.is_none() {
            return Err(Refusal::new(format!(
                "the later step for {} names neither a model nor an effort",
                step.kind
            )));
        }
        if steps[..i].iter().any(|s| s.kind == step.kind) {
            return Err(Refusal::new(format!(
                "later_steps names {} twice; give one entry per kind",
                step.kind
            )));
        }
    }
    Ok(())
}

/// One task up the `builds_on` chain, with the entries recorded on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainLink<'a> {
    pub task: TaskId,
    pub later_steps: &'a [LaterStep],
    /// Your message the entries were recorded from.
    pub message: Option<MessageId>,
}

/// The entry applied to a new task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedLaterStep {
    pub from: TaskId,
    pub message: Option<MessageId>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
}

/// Finds the entry for a new task of `kind`. `chain` is the `builds_on` chain, nearest task
/// first ("directly or further down the chain"); the nearest entry wins, since it was recorded
/// from newer words of yours. A stronger-model retry you picked takes precedence, so none
/// applies to it.
pub fn applicable(
    kind: TaskKind,
    stronger_retry: bool,
    chain: &[ChainLink<'_>],
) -> Option<AppliedLaterStep> {
    if stronger_retry {
        return None;
    }
    chain.iter().find_map(|link| {
        link.later_steps
            .iter()
            .find(|s| s.kind == kind)
            .map(|s| AppliedLaterStep {
                from: link.task,
                message: link.message,
                model: s.model.clone(),
                effort: s.effort,
            })
    })
}
