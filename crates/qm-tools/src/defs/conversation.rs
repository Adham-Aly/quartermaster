//! Talking to you: the Quartermaster and secondmates speak only through these.

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
