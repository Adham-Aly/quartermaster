//! Our tools as an engine sees them: what to offer the model, and what comes back.

use serde::{Deserialize, Serialize};

/// One of our tools, offered to the model exactly as `qm-tools` defines it: both engines see the
/// same name, description and schema. On Claude the engine serves these from its in-process MCP
/// server as `mcp__quartermaster__<name>`; the provider engine sends them in each request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// The engine's ID for one tool call, echoed back with its result.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a tool call returns to the model: text (our tools return JSON or a plain sentence), and
/// whether it is an error. Refusals are errors with the rule's plain reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}
