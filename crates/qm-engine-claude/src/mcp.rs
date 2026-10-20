//! Our tools as an in-process MCP server (`sdkMcpServers: ["quartermaster"]`).
//!
//! `claude` reaches the server through `mcp_message` control requests. `initialize`, `ping` and
//! `tools/list` are answered here; `tools/call` becomes the engine's `ToolCall`, answered later
//! from the core's `ToolResult`. Notifications get the SDK's fixed `{"jsonrpc":"2.0","result":{},
//! "id":0}`; an unknown method gets JSON-RPC `-32601`. The same answers go up front in
//! `initialize.sdkMcpServerManifests`, as the SDK sends them, so `claude` can skip the round trips.

use crate::SERVER_NAME;
use qm_engine::{ToolDescriptor, ToolResult};
use serde_json::{Value, json};

/// The MCP protocol version the SDK speaks (`sdk.mjs` `Y4`).
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

/// The server's version as it reports itself.
pub const SERVER_VERSION: &str = "0.1.0";

/// What one MCP message asks of the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum McpOutcome {
    /// Answer at once with this JSON-RPC message.
    Reply(Value),
    /// A call to one of our tools: the core runs it.
    Call {
        /// The JSON-RPC ID to answer with.
        id: Value,
        name: String,
        args: Value,
        /// `_meta["claudecode/toolUseId"]`, the model's tool-use ID, when `claude` sends it.
        tool_use_id: Option<String>,
    },
}

/// The in-process server for one session.
#[derive(Clone, Debug)]
pub struct McpServer {
    tools: Vec<ToolDescriptor>,
}

impl McpServer {
    pub fn new(tools: Vec<ToolDescriptor>) -> Self {
        Self { tools }
    }

    /// Our tools as `claude` names them: `mcp__quartermaster__<name>`.
    pub fn qualified_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| format!("mcp__{SERVER_NAME}__{}", t.name))
            .collect()
    }

    pub fn initialize_result(protocol_version: Option<&str>) -> Value {
        json!({
            "protocolVersion": protocol_version.unwrap_or(MCP_PROTOCOL_VERSION),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION}
        })
    }

    /// `tools/list`: each tool as the SDK sends it.
    pub fn tools_list(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "execution": {"taskSupport": "forbidden"}
                })
            })
            .collect();
        json!({ "tools": tools })
    }

    /// `initialize.sdkMcpServerManifests`.
    pub fn manifests(&self) -> Value {
        json!({ SERVER_NAME: {
            "initializeResult": Self::initialize_result(Some(MCP_PROTOCOL_VERSION)),
            "toolsListResult": self.tools_list()
        }})
    }

    /// Handles one JSON-RPC message from `claude`.
    pub fn handle(&self, message: &Value) -> McpOutcome {
        let Some(id) = message.get("id").cloned().filter(|v| !v.is_null()) else {
            // A notification (or a stray response): the SDK's fixed answer.
            return McpOutcome::Reply(json!({"jsonrpc": "2.0", "result": {}, "id": 0}));
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => Ok(Self::initialize_result(
                message
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str),
            )),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if self.tools.iter().any(|t| t.name == name) {
                    return McpOutcome::Call {
                        id,
                        args: message
                            .pointer("/params/arguments")
                            .cloned()
                            .unwrap_or_else(|| json!({})),
                        tool_use_id: message
                            .pointer("/params/_meta/claudecode~1toolUseId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        name,
                    };
                }
                Err((-32602, format!("unknown tool {name}")))
            }
            _ => Err((-32601, format!("Method not found: {method}"))),
        };
        McpOutcome::Reply(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// The JSON-RPC answer carrying a tool's result.
    pub fn call_result(id: &Value, result: &ToolResult) -> Value {
        let mut body = json!({"content": [{"type": "text", "text": result.text}]});
        if result.is_error {
            body["isError"] = json!(true);
        }
        json!({"jsonrpc": "2.0", "id": id, "result": body})
    }
}
