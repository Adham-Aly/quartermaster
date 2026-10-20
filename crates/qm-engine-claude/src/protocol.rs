//! The stream-json control protocol: what we write, and a tolerant reading of what `claude` writes.
//!
//! Outgoing messages follow the Agent SDK 0.3.302 byte for byte where it matters
//! (`buildInitializeRequest`, the user message `Yx`, control responses). Incoming lines are
//! classified by `type` (and `subtype`) only; every field is read optionally, unknown types and
//! fields are ignored, and a line that isn't JSON is skipped. Nothing here is ever fatal.

use base64::Engine as _;
use qm_engine::{InputBlock, TurnInput};
use serde_json::{Map, Value, json};

/// The `initialize` control request (`buildInitializeRequest`; fields left undefined are
/// omitted, as `JSON.stringify` would).
pub fn initialize(
    request_id: &str,
    server: &str,
    manifests: Option<Value>,
    system_prompt: Option<&str>,
    append_system_prompt: Option<&str>,
) -> Value {
    let mut request = Map::new();
    request.insert("subtype".into(), json!("initialize"));
    request.insert("sdkMcpServers".into(), json!([server]));
    if let Some(manifests) = manifests {
        request.insert("sdkMcpServerManifests".into(), manifests);
    }
    if let Some(prompt) = system_prompt {
        request.insert("systemPrompt".into(), json!([prompt]));
    }
    if let Some(prompt) = append_system_prompt {
        request.insert("appendSystemPrompt".into(), json!(prompt));
    }
    json!({"request_id": request_id, "type": "control_request", "request": Value::Object(request)})
}

/// Any other control request we send: `{"subtype": …, …}` under `request`.
pub fn control_request(request_id: &str, request: Value) -> Value {
    let mut msg = Map::new();
    msg.insert("request_id".into(), json!(request_id));
    msg.insert("type".into(), json!("control_request"));
    msg.insert("request".into(), request);
    Value::Object(msg)
}

/// A successful answer to one of `claude`'s control requests.
pub fn control_success(request_id: &Value, response: Value) -> Value {
    let mut body = Map::new();
    body.insert("subtype".into(), json!("success"));
    body.insert("request_id".into(), request_id.clone());
    body.insert("response".into(), response);
    json!({"type": "control_response", "response": Value::Object(body)})
}

/// A refusal of one of `claude`'s control requests.
pub fn control_error(request_id: &Value, error: &str) -> Value {
    json!({"type": "control_response",
           "response": {"subtype": "error", "request_id": request_id, "error": error}})
}

/// A user message carrying a turn's input (the SDK's `Yx`). Images are read from their files and
/// attached as base64.
pub fn user_message(input: &TurnInput, uuid: &str) -> Result<Value, String> {
    let mut content = Vec::with_capacity(input.blocks.len());
    for block in &input.blocks {
        match block {
            InputBlock::Text { text } => content.push(json!({"type": "text", "text": text})),
            InputBlock::Image { path, media_type } => {
                let bytes = std::fs::read(path)
                    .map_err(|e| format!("couldn't read the image {}: {e}", path.display()))?;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                content.push(json!({"type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data}}));
            }
        }
    }
    // `uuid` names the message in `claude`'s `command_lifecycle` reports (queued, started,
    // completed), which tell the engine whether a message sent mid-turn is still to be answered.
    Ok(json!({
        "type": "user",
        "session_id": "",
        "message": {"role": "user", "content": content},
        "parent_tool_use_id": null,
        "uuid": uuid
    }))
}

/// One line from `claude`, classified.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// An answer to one of our control requests.
    ControlResponse {
        request_id: String,
        /// The response body, or the error text.
        result: Result<Value, String>,
    },
    /// `claude` asks us something (`mcp_message`, …). `request_id` is echoed back verbatim.
    ControlRequest { request_id: Value, request: Value },
    /// `claude` withdrew one of its control requests.
    ControlCancel { request_id: String },
    /// `type: system`, by subtype.
    System { subtype: String, msg: Value },
    /// A partial-message stream event; `parent` is set for a sub-agent's stream.
    StreamEvent {
        event: Value,
        parent: Option<String>,
    },
    /// A whole assistant message.
    Assistant {
        message: Value,
        parent: Option<String>,
    },
    /// A user message echoed by `claude` (tool results).
    User { msg: Value, parent: Option<String> },
    /// The end of a turn.
    Result(Value),
    /// A rate-limit report.
    RateLimit(Value),
    /// A built-in tool's progress report.
    ToolProgress(Value),
    /// Anything else: ignored, logged at debug.
    Other(String),
}

/// Classifies one stdout line. `None` for a blank line or one that isn't a JSON object.
pub fn classify(line: &[u8]) -> Option<(Value, Incoming)> {
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    let msg: Value = serde_json::from_slice(trimmed).ok()?;
    if !msg.is_object() {
        return None;
    }
    let incoming = classify_value(&msg);
    Some((msg, incoming))
}

/// Classifies an already parsed message.
pub fn classify_value(msg: &Value) -> Incoming {
    let ty = str_at(msg, "/type").unwrap_or_default();
    let parent = str_at(msg, "/parent_tool_use_id").map(str::to_owned);
    match ty {
        "control_response" => {
            let request_id = str_at(msg, "/response/request_id")
                .unwrap_or_default()
                .to_owned();
            let result = if str_at(msg, "/response/subtype") == Some("success") {
                Ok(msg
                    .pointer("/response/response")
                    .cloned()
                    .unwrap_or(Value::Null))
            } else {
                Err(msg.pointer("/response/error").map_or_else(
                    || "the request failed".into(),
                    |e| e.as_str().map_or_else(|| e.to_string(), str::to_owned),
                ))
            };
            Incoming::ControlResponse { request_id, result }
        }
        "control_request" => Incoming::ControlRequest {
            request_id: msg.get("request_id").cloned().unwrap_or(Value::Null),
            request: msg.get("request").cloned().unwrap_or(Value::Null),
        },
        "control_cancel_request" => Incoming::ControlCancel {
            request_id: str_at(msg, "/request_id").unwrap_or_default().to_owned(),
        },
        "system" => Incoming::System {
            subtype: str_at(msg, "/subtype").unwrap_or_default().to_owned(),
            msg: msg.clone(),
        },
        "stream_event" => Incoming::StreamEvent {
            event: msg.get("event").cloned().unwrap_or(Value::Null),
            parent,
        },
        "assistant" => Incoming::Assistant {
            message: msg.get("message").cloned().unwrap_or(Value::Null),
            parent,
        },
        "user" => Incoming::User {
            msg: msg.clone(),
            parent,
        },
        "result" => Incoming::Result(msg.clone()),
        "rate_limit_event" => Incoming::RateLimit(msg.clone()),
        "tool_progress" => Incoming::ToolProgress(msg.clone()),
        other => Incoming::Other(other.to_owned()),
    }
}

/// A string at a JSON pointer, if it is one.
pub fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

/// An unsigned number at a JSON pointer, `0` when absent.
pub fn u64_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

/// Replaces every `account` object anywhere in `value` with `"<redacted>"`.
pub fn redact_accounts(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "account" && !child.is_string() && !child.is_null() {
                    *child = json!("<redacted>");
                } else {
                    redact_accounts(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_accounts),
        _ => {}
    }
}
