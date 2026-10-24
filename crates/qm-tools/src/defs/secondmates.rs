//! Secondmates: proposing, routing to and configuring them, and their parent channel.

use super::{Ack, Held, LaterStep};
use qm_types::domain::Effort;
use qm_types::ids::{CorrelationId, ModelId, SecondmateId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Proposes a secondmate; always a held action for your confirmation.
    StartSecondmateTool {
        name: "start_secondmate",
        class: Act,
        roles: [Quartermaster],
        args: StartSecondmateArgs,
        output: Held,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartSecondmateArgs {
    pub name: String,
    // The scope words, e.g. "web-app frontend".
    pub scope: String,
    // A standing base branch: only on a turn you started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

tool! {
    /// Sends a request into one of the Quartermaster's own secondmates' scope; the app stamps the
    /// marker and a correlation ID, and carries your words, the one-time instructions, and
    /// overrides with user authority (model choices may cite an earlier actual user message).
    RouteToSecondmateTool {
        name: "route_to_secondmate",
        class: Act,
        roles: [Quartermaster],
        args: RouteToSecondmateArgs,
        output: Routed,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteToSecondmateArgs {
    pub secondmate: SecondmateId,
    pub request: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<String>,
    // Stored overrides need user authority; model/effort/premium may cite an earlier message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
    // The actual user message authorizing model/effort/premium, including on a later turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<i64>")]
    pub choice_message: Option<qm_types::ids::MessageId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later_steps: Vec<LaterStep>,
}

/// A request was routed; its answer comes back on the parent channel with this ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routed {
    pub correlation: CorrelationId,
}

tool! {
    /// Changes a secondmate's charter or standing base branch (the branch only on your turns);
    /// affects only crewmates started afterwards.
    ConfigureSecondmateTool {
        name: "configure_secondmate",
        class: Act,
        roles: [Quartermaster],
        args: ConfigureSecondmateArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureSecondmateArgs {
    pub secondmate: SecondmateId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    // Set or clear the standing branch (not both); only on a turn you started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_standing_base_branch: bool,
}

tool! {
    /// Retires one of the Quartermaster's own secondmates once it has no open tasks;
    /// history is not a permanent archive and numbers may be reused after restart.
    CloseSecondmateTool {
        name: "close_secondmate",
        class: Act,
        roles: [Quartermaster],
        args: CloseSecondmateArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseSecondmateArgs {
    pub secondmate: SecondmateId,
}

tool! {
    /// A secondmate's judgment on the parent channel; the app writes the facts and wakes the
    /// Quartermaster.
    ParentReportTool {
        name: "parent_report",
        class: Act,
        roles: [Secondmate],
        args: ParentReportArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParentReportArgs {
    pub correlation: CorrelationId,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_user: Option<String>,
}
