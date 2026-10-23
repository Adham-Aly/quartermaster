//! Pieces several tools share.

use qm_types::domain::{Effort, TaskKind, TaskStatus, Tier};
use qm_types::ids::{HeldActionId, MessageId, ModelId, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// A model, effort or premium you named for a later step of a workflow, recorded on your turn and
// applied by the app when a later task of that kind builds on this one. (Argument types use `//`
// comments: a doc comment would become a nested schema description.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaterStep {
    pub kind: TaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    // Premium for this step, only when you asked for it on later steps.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
}

// A limit the work must respect, shown in the brief; `freeze` lists paths the crewmate must not
// change (the brief's "[structured: freeze=/auth/*]").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freeze: Vec<String>,
}

/// A chat line was shown: `reply`, `ask_user`, `acknowledge`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageShown {
    pub message: MessageId,
}

/// Something is waiting for the user's yes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub held_action: HeldActionId,
    /// One plain sentence for the agent, written by the app.
    pub note: String,
}

/// A call that changed something and has nothing more to return than a plain sentence from the
/// app saying what happens next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub note: String,
}

/// One task in a list, as the context block shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task: TaskId,
    pub project: String,
    pub kind: TaskKind,
    pub title: String,
    pub status: TaskStatus,
    /// The one-line state, e.g. "PR #212 ready · 14/14 · held for your approval @a1b2c3d".
    pub line: String,
}

/// The routing decision for a started task, with its plain-language "why".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingSummary {
    /// The tier the work runs at.
    pub tier: Tier,
    /// `None` while the task waits for a model (premium requests and stronger-model retries
    /// wait rather than drop a tier).
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub why: String,
}
