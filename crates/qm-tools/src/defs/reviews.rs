//! Review tools: the orchestrators' `approve_review`, the user's go-ahead on a review's findings
//! under normal autonomy.

use qm_types::ids::TaskId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Records the user's go-ahead on a review's findings; only on a turn the user's own message
    /// started.
    ApproveReviewTool {
        name: "approve_review",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: ApproveReviewArgs,
        output: super::Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveReviewArgs {
    pub task: TaskId,
}
