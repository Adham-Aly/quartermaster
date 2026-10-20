//! Our message model: what a provider session's context is made of.
//!
//! Adapter-agnostic: every wire adapter converts these to its provider's request format and builds
//! them back from its stream, and the session file stores them as they are. An assistant message
//! remembers which provider, API and model wrote it, so the replay rules ([`crate::replay`]) can
//! drop what another model can't read.
//!
//! Serialised with serde for the session file (`role`-tagged); image bytes are base64 there.

use crate::adapters::WireApi;
use qm_engine::{CallUsage, StopReason};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use serde::{Deserialize, Serialize};

/// One message of the conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// A turn's input (or an injected message, or the compaction summary).
    User { content: Vec<Block> },
    /// One model reply.
    Assistant {
        content: Vec<AssistantBlock>,
        provider: Provider,
        api: WireApi,
        model: ModelId,
        stop: StopReason,
        /// Cut short by an interrupt: kept in the file, skipped on replay.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        aborted: bool,
        /// Ended in an error: kept in the file, skipped on replay.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        failed: bool,
        #[serde(default)]
        usage: CallUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
    },
    /// What one tool call returned.
    ToolResult {
        call_id: String,
        name: String,
        content: Vec<Block>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// A block of user or tool-result content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// A block of an assistant reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
        /// The provider's handle for replaying this text to the same model (Responses: the
        /// output message's ID and phase, as JSON).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Thinking {
        text: String,
        /// The provider's encrypted reasoning, for replay to the same model only (Responses: the
        /// whole reasoning item, as JSON).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall {
        /// The provider's call ID (Responses: `call_id|item_id`).
        id: String,
        name: String,
        /// The parsed arguments: an object. Arguments that weren't valid JSON are kept as the raw
        /// string, and the loop answers that call with an error result.
        args: serde_json::Value,
        /// The provider's opaque signature for replaying this call to the same model only
        /// (Gemini: the function call's `thoughtSignature`); the replay rules drop it for any
        /// other provider, API or model. Older session files have none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_signature: Option<String>,
    },
}

impl Message {
    /// A user message with one text block.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![Block::text(text)],
        }
    }

    /// An assistant message's tool calls, in order.
    pub fn tool_calls(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        match self {
            Self::Assistant { content, .. } => content
                .iter()
                .filter_map(|b| match b {
                    AssistantBlock::ToolCall { id, name, args, .. } => {
                        Some((id.as_str(), name.as_str(), args))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The plain text of an assistant message (its text blocks, joined), or of a user message.
    pub fn text(&self) -> String {
        match self {
            Self::Assistant { content, .. } => content
                .iter()
                .filter_map(|b| match b {
                    AssistantBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            Self::User { content } | Self::ToolResult { content, .. } => content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    Block::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Whether this is an assistant reply that must not be replayed (aborted or failed).
    pub fn is_unreplayable(&self) -> bool {
        matches!(self, Self::Assistant { aborted, failed, .. } if *aborted || *failed)
    }

    pub fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
    }
}

/// Whether tool-call arguments are usable: the loop answers any other with an error result.
pub fn args_are_valid(args: &serde_json::Value) -> bool {
    args.is_object()
}

mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}
