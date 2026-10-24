//! Conversation tools: speaking to you and finding original user requests.

use super::MessageShown;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// A full answer, shown in chat. The app checks it isn't empty.
    ReplyTool {
        name: "reply",
        class: Speak,
        roles: [Quartermaster, Secondmate],
        args: ReplyArgs,
        output: MessageShown,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyArgs {
    // Markdown; must not be empty.
    pub text: String,
}

tool! {
    /// A question card; your answer arrives as your next message.
    AskUserTool {
        name: "ask_user",
        class: Speak,
        roles: [Quartermaster, Secondmate],
        args: AskUserArgs,
        output: MessageShown,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserArgs {
    pub question: String,
    // One-click answers; you can always type your own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

tool! {
    /// A few words beside your message, at most about 40 characters; never stored as an answer.
    AcknowledgeTool {
        name: "acknowledge",
        class: Speak,
        roles: [Quartermaster, Secondmate],
        args: AcknowledgeArgs,
        output: MessageShown,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeArgs {
    // At most about 40 characters (checked by the handler).
    pub text: String,
}

tool! {
    /// Finds immutable original user requests in the caller's own conversation.
    FindUserRequestsTool {
        name: "find_user_requests",
        class: Read,
        roles: [Quartermaster, Secondmate],
        args: FindUserRequestsArgs,
        output: FoundUserRequests,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindUserRequestsArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<i64>")]
    pub before: Option<qm_types::ids::MessageId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FoundUserRequests {
    pub requests: Vec<UserRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserRequest {
    pub message: i64,
    pub text: String,
}
