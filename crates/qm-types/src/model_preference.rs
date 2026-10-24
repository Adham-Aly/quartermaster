//! A model-selection decision, constrained to the task's routed tier.
use crate::domain::{Effort, Provider};
use crate::ids::ModelId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The user's requirements and the decision maker's strongest matching model.
/// Today the Quartermaster supplies this; an external decision model can supply the same
/// answer later. Routing always revalidates the answer against live, eligible tier entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPreference {
    pub selection: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}
