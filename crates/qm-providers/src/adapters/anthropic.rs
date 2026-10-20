//! Anthropic Messages.
//!
//! A Rust port of Pi's `packages/ai/src/api/anthropic-messages.ts` and the parts of
//! `transform-messages.ts` it uses, at the pinned commit `1402973a` (MIT, Copyright Earendil
//! Works), for an Anthropic API key (Claude subscriptions run on the Claude engine).
//!
//! - **Request**: `POST <base>/v1/messages` on `https://api.anthropic.com`, with
//!   `anthropic-version: 2023-06-01` and the key in `x-api-key`
//!   ([`WireAdapter::credential_header`]). Body: `model`, `messages`, `max_tokens` (the
//!   orchestrators' cap, else the model's limit: the API requires it), `stream: true`, the system
//!   prompt as one text block, `tools` (`input_schema` reduced to `type`, `properties` and
//!   `required`, with `eager_input_streaming` unless the catalog says the model lacks it), and
//!   the thinking settings. Prompt caching as Pi's default ("short"): an ephemeral
//!   `cache_control` on the system block, the last tool and the last user block.
//! - **Effort** (through the catalog): a rung's provider value goes in
//!   `output_config.effort` with adaptive thinking (`display: "summarized"`); a rung mapped to
//!   `off` sends `thinking: {type: "disabled"}`; an unsupported rung sends no effort (adaptive
//!   thinking at the model's default).
//! - **Replay**: thinking keeps Anthropic's signature as the block's signature (the raw string) and
//!   goes back as a `thinking` block to the same model; redacted thinking keeps its opaque `data`
//!   and goes back as `redacted_thinking`; same-model thinking that lost its signature (an
//!   interrupted stream) goes back as text. Tool-call IDs from another model are sanitised to
//!   `[A-Za-z0-9_-]`, at most 64 characters. Consecutive tool results share one user message; an
//!   error result's images follow all the results, since Anthropic refuses images inside an error
//!   result.
//! - **Stream**: `message_start`, `content_block_start` / `_delta` / `_stop`, `message_delta`,
//!   `message_stop` (terminal), `error`; `ping` and anything unknown is ignored. A stream that
//!   ends before `message_stop`, without a stop reason, or with a tool call never stopped is an
//!   error. `refusal` and `sensitive` stops are errors that aren't retried.
//!
//! Not ported (not used by the accounts we serve): Claude Code OAuth tokens and their tool-name
//! casing, Copilot and OpenRouter through this API (they use Chat Completions), budget-based
//! thinking for pre-adaptive models, mid-conversation effort and tool changes, strict tools,
//! server-side fallbacks.

use super::common::{self, EffortSetting};
use super::responses::{header_str, parse_event_json, parse_tool_arguments};
use super::{
    AdapterError, ClassifiedError, CredentialHeader, CredentialKind, ErrorClass, HeaderMap,
    HttpRequest, ModelRequest, StreamEvent, StreamParser, WireAdapter, WireApi,
};
use crate::sse::SseEvent;
use crate::transcript::{AssistantBlock, Block, Message};
use qm_engine::{CallUsage, ProviderErrorKind, StopReason};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Anthropic's default base URL.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// The API version every request names.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The beta that streams tool input in fine-grained chunks, for models without eager input
/// streaming.
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
/// What redacted thinking shows as.
pub const REDACTED_THINKING_TEXT: &str = "[Reasoning redacted]";
/// What an error result's images become inside it.
const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";
/// Catalog compatibility flag: tools take `eager_input_streaming` (default on).
pub const COMPAT_EAGER_TOOL_INPUT: &str = "supports_eager_tool_input_streaming";
/// Catalog compatibility flag: the last tool may carry `cache_control` (default on).
pub const COMPAT_CACHE_CONTROL_ON_TOOLS: &str = "supports_cache_control_on_tools";

/// The Anthropic Messages adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicAdapter;

impl AnthropicAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn url(base_url: Option<&str>) -> String {
        let base = base_url
            .filter(|b| !b.trim().is_empty())
            .unwrap_or(ANTHROPIC_BASE_URL)
            .trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }
}

fn flag(compat: &BTreeMap<String, bool>, name: &str, default: bool) -> bool {
    compat.get(name).copied().unwrap_or(default)
}

impl WireAdapter for AnthropicAdapter {
    fn api(&self) -> WireApi {
        WireApi::AnthropicMessages
    }

    fn build_request(&self, req: &ModelRequest<'_>) -> Result<HttpRequest, AdapterError> {
        let compat = common::compat_flags(req.provider, &req.model.id);
        let eager_input = flag(&compat, COMPAT_EAGER_TOOL_INPUT, true);
        let cache_on_tools = flag(&compat, COMPAT_CACHE_CONTROL_ON_TOOLS, true);
        let cache_control = json!({ "type": "ephemeral" });

        let mut body = Map::new();
        body.insert("model".into(), json!(req.model.id.as_str()));
        body.insert(
            "messages".into(),
            Value::Array(convert_messages(req.messages, &cache_control)),
        );
        let model_max = req.model.max_output_tokens.max(1);
        let max_tokens = req
            .max_output_tokens
            .filter(|_| self.can_send_output_cap(req))
            .map_or(model_max, |cap| cap.clamp(1, model_max));
        body.insert("max_tokens".into(), json!(max_tokens));
        body.insert("stream".into(), json!(true));
        if !req.system_prompt.is_empty() {
            body.insert(
                "system".into(),
                json!([{
                    "type": "text",
                    "text": req.system_prompt,
                    "cache_control": cache_control,
                }]),
            );
        }
        if !req.tools.is_empty() {
            body.insert(
                "tools".into(),
                convert_tools(req, eager_input, cache_on_tools.then_some(&cache_control)),
            );
        }
        match common::effort_setting(req) {
            EffortSetting::Off => {
                body.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            EffortSetting::Value(effort) => {
                body.insert(
                    "thinking".into(),
                    json!({ "type": "adaptive", "display": "summarized" }),
                );
                body.insert("output_config".into(), json!({ "effort": effort }));
            }
            EffortSetting::Omit if req.model.reasoning => {
                body.insert(
                    "thinking".into(),
                    json!({ "type": "adaptive", "display": "summarized" }),
                );
            }
            EffortSetting::Omit => {}
        }

        let mut headers = vec![
            ("User-Agent".to_owned(), req.user_agent.to_owned()),
            ("accept".to_owned(), "application/json".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
        ];
        if !req.tools.is_empty() && !eager_input {
            headers.push((
                "anthropic-beta".to_owned(),
                FINE_GRAINED_TOOL_STREAMING_BETA.to_owned(),
            ));
        }
        Ok(HttpRequest {
            url: Self::url(req.base_url),
            headers,
            body: Value::Object(body),
        })
    }

    fn parser(&self, req: &ModelRequest<'_>) -> Box<dyn StreamParser> {
        Box::new(AnthropicParser::new(req.provider, req.model.id.clone()))
    }

    fn classify_error(&self, status: u16, headers: &HeaderMap, body: &str) -> ClassifiedError {
        classify_http_error(status, headers, body)
    }

    fn normalize_tool_call_id(&self, id: &str, _from_foreign_model: bool) -> String {
        normalize_tool_call_id(id)
    }

    fn can_send_output_cap(&self, _req: &ModelRequest<'_>) -> bool {
        true
    }

    fn credential_header(&self, credential: &CredentialKind) -> CredentialHeader {
        match credential {
            CredentialKind::ApiKey => CredentialHeader::Named("x-api-key"),
            CredentialKind::SignInWithChatGpt | CredentialKind::Codex { .. } => {
                CredentialHeader::Bearer
            }
        }
    }
}

/// Pi's rule: every character outside `[A-Za-z0-9_-]` becomes `_`, then the first 64 are kept.
pub fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

// =============================================================================================
// Request conversion
// =============================================================================================

/// The `tools` array (Pi's `convertTools`, without strict tools).
fn convert_tools(req: &ModelRequest<'_>, eager_input: bool, cache: Option<&Value>) -> Value {
    let last = req.tools.len().saturating_sub(1);
    Value::Array(
        req.tools
            .iter()
            .enumerate()
            .map(|(i, tool)| {
                let schema = &tool.input_schema;
                let mut out = Map::new();
                out.insert("name".into(), json!(tool.name));
                out.insert("description".into(), json!(tool.description));
                if eager_input {
                    out.insert("eager_input_streaming".into(), json!(true));
                }
                out.insert(
                    "input_schema".into(),
                    json!({
                        "type": "object",
                        "properties": schema.get("properties").cloned().unwrap_or_else(|| json!({})),
                        "required": schema.get("required").cloned().unwrap_or_else(|| json!([])),
                    }),
                );
                if let Some(cache) = cache
                    && i == last
                {
                    out.insert("cache_control".into(), cache.clone());
                }
                Value::Object(out)
            })
            .collect(),
    )
}

fn image_block(media_type: &str, bytes: &[u8]) -> Value {
    use base64::Engine as _;
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    })
}

/// A tool result's content: plain text when there are no images, else text and image blocks
/// (with a note first when there is no text).
fn tool_result_content(content: &[Block]) -> Value {
    let has_images = content.iter().any(|b| matches!(b, Block::Image { .. }));
    if !has_images {
        let text = content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                Block::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return json!(text);
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .map(|b| match b {
            Block::Text { text } => json!({ "type": "text", "text": text }),
            Block::Image { media_type, bytes } => image_block(media_type, bytes),
        })
        .collect();
    if !content.iter().any(|b| matches!(b, Block::Text { .. })) {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

/// An error result's images, replaced in its content by a note (each run of images by one).
fn split_error_images(content: &[Block]) -> (Vec<Block>, Vec<(&str, &[u8])>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            Block::Image { media_type, bytes } => {
                if !previous_was_placeholder {
                    text.push(Block::text(ERROR_TOOL_IMAGE_PLACEHOLDER));
                }
                previous_was_placeholder = true;
                images.push((media_type.as_str(), bytes.as_slice()));
            }
            Block::Text { text: t } => {
                previous_was_placeholder = t == ERROR_TOOL_IMAGE_PLACEHOLDER;
                text.push(block.clone());
            }
        }
    }
    (text, images)
}

/// One assistant message's blocks (Pi's `convertMessages`, assistant branch).
fn convert_assistant(content: &[AssistantBlock]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            AssistantBlock::Text { text, .. } => {
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            AssistantBlock::Thinking {
                text,
                signature,
                redacted,
            } => {
                if *redacted {
                    blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": signature.clone().unwrap_or_default(),
                    }));
                    continue;
                }
                let signature = signature.as_deref().filter(|s| !s.trim().is_empty());
                match signature {
                    None if text.trim().is_empty() => {}
                    // A same-model block whose signature never arrived goes back as text.
                    None => blocks.push(json!({ "type": "text", "text": text })),
                    Some(signature) => blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature,
                    })),
                }
            }
            AssistantBlock::ToolCall { id, name, args, .. } => blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": common::args_object(args),
            })),
        }
    }
    blocks
}

/// Our prepared transcript as Anthropic messages, with `cache_control` on the last user block.
fn convert_messages(messages: &[Message], cache_control: &Value) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::User { content } => {
                let blocks: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } if text.trim().is_empty() => None,
                        Block::Text { text } => Some(json!({ "type": "text", "text": text })),
                        Block::Image { media_type, bytes } => Some(image_block(media_type, bytes)),
                    })
                    .collect();
                if !blocks.is_empty() {
                    params.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Message::Assistant { content, .. } => {
                let blocks = convert_assistant(content);
                if !blocks.is_empty() {
                    params.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Message::ToolResult { .. } => {
                let mut results = Vec::new();
                let mut error_images = Vec::new();
                while let Some(Message::ToolResult {
                    call_id,
                    name,
                    content,
                    is_error,
                }) = messages.get(i)
                {
                    let has_images = content.iter().any(|b| matches!(b, Block::Image { .. }));
                    if *is_error && has_images {
                        let (text, images) = split_error_images(content);
                        results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": tool_result_content(&text),
                            "is_error": true,
                        }));
                        error_images.push(json!({
                            "type": "text",
                            "text": format!(
                                "Images from the error result of tool call {call_id} ({name}):"
                            ),
                        }));
                        error_images.extend(images.into_iter().map(|(m, b)| image_block(m, b)));
                    } else {
                        results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": tool_result_content(content),
                            "is_error": is_error,
                        }));
                    }
                    i += 1;
                }
                results.extend(error_images);
                params.push(json!({ "role": "user", "content": results }));
                continue;
            }
        }
        i += 1;
    }
    add_cache_control(&mut params, cache_control);
    params
}

/// `cache_control` on the last block of the last message, when that message is the user's.
fn add_cache_control(params: &mut [Value], cache_control: &Value) {
    let Some(last) = params.last_mut() else {
        return;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return;
    }
    if let Some(block) = last
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
        && matches!(
            block.get("type").and_then(Value::as_str),
            Some("text" | "image" | "tool_result")
        )
        && let Some(block) = block.as_object_mut()
    {
        block.insert("cache_control".into(), cache_control.clone());
    }
}

// =============================================================================================
// Stream parsing
// =============================================================================================

#[derive(Debug)]
enum Partial {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        signature: String,
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        json: String,
        /// The input `content_block_start` carried, used when nothing streamed.
        start_input: Value,
        /// Set once `content_block_stop` parsed the arguments.
        args: Option<Value>,
    },
}

/// Parses one Anthropic Messages stream (Pi's `stream`).
#[derive(Debug)]
pub struct AnthropicParser {
    provider: Provider,
    model: ModelId,
    /// Each block with the stream index it has while open.
    blocks: Vec<(Option<u64>, Partial)>,
    response_id: Option<String>,
    usage: CallUsage,
    stop: Option<StopReason>,
    /// A stop reason that is an error (`refusal`, `sensitive`, an unknown one), reported at the
    /// end of the stream.
    stop_error: Option<ClassifiedError>,
    started: bool,
    terminal: bool,
    finished: bool,
}

impl AnthropicParser {
    pub fn new(provider: Provider, model: ModelId) -> Self {
        Self {
            provider,
            model,
            blocks: Vec::new(),
            response_id: None,
            usage: CallUsage::default(),
            stop: None,
            stop_error: None,
            started: false,
            terminal: false,
            finished: false,
        }
    }

    fn open_block(&mut self, index: u64) -> Option<&mut Partial> {
        self.blocks
            .iter_mut()
            .find(|(i, _)| *i == Some(index))
            .map(|(_, p)| p)
    }

    fn block_start(&mut self, index: u64, block: &Value, out: &mut Vec<StreamEvent>) {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let partial = match kind {
            "text" => {
                let text = str_field(block, "text");
                if !text.is_empty() {
                    out.push(StreamEvent::TextDelta(text.clone()));
                }
                Partial::Text { text }
            }
            "thinking" => {
                let text = str_field(block, "thinking");
                if !text.is_empty() {
                    out.push(StreamEvent::ThinkingDelta(text.clone()));
                }
                Partial::Thinking {
                    text,
                    signature: str_field(block, "signature"),
                    redacted: false,
                }
            }
            "redacted_thinking" => Partial::Thinking {
                text: REDACTED_THINKING_TEXT.to_owned(),
                signature: str_field(block, "data"),
                redacted: true,
            },
            "tool_use" => {
                let id = str_field(block, "id");
                let name = str_field(block, "name");
                out.push(StreamEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                Partial::ToolCall {
                    id,
                    name,
                    json: String::new(),
                    start_input: block.get("input").cloned().unwrap_or(Value::Null),
                    args: None,
                }
            }
            other => {
                tracing::debug!(
                    block_type = other,
                    "anthropic: ignoring a content block type"
                );
                return;
            }
        };
        self.blocks.push((Some(index), partial));
    }

    fn block_delta(&mut self, index: u64, delta: &Value, out: &mut Vec<StreamEvent>) {
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(block) = self.open_block(index) else {
            return;
        };
        match (kind, block) {
            ("text_delta", Partial::Text { text }) => {
                let piece = str_field(delta, "text");
                text.push_str(&piece);
                out.push(StreamEvent::TextDelta(piece));
            }
            ("thinking_delta", Partial::Thinking { text, .. }) => {
                let piece = str_field(delta, "thinking");
                text.push_str(&piece);
                out.push(StreamEvent::ThinkingDelta(piece));
            }
            ("input_json_delta", Partial::ToolCall { json, .. }) => {
                let piece = str_field(delta, "partial_json");
                json.push_str(&piece);
                out.push(StreamEvent::ToolCallArgsDelta(piece));
            }
            ("signature_delta", Partial::Thinking { signature, .. }) => {
                signature.push_str(&str_field(delta, "signature"));
            }
            (other, _) => {
                tracing::debug!(delta_type = other, "anthropic: ignoring a content delta");
            }
        }
    }

    fn block_stop(&mut self, index: u64, out: &mut Vec<StreamEvent>) {
        let Some((open, block)) = self.blocks.iter_mut().find(|(i, _)| *i == Some(index)) else {
            return;
        };
        *open = None;
        match block {
            Partial::Text { .. } => out.push(StreamEvent::TextEnd),
            Partial::Thinking {
                text,
                signature,
                redacted,
            } => out.push(StreamEvent::ThinkingEnd(AssistantBlock::Thinking {
                text: text.clone(),
                signature: Some(signature.clone()).filter(|s| !s.is_empty()),
                redacted: *redacted,
            })),
            Partial::ToolCall {
                id,
                name,
                json,
                start_input,
                args,
            } => {
                let parsed = if json.trim().is_empty() && start_input.is_object() {
                    start_input.clone()
                } else {
                    parse_tool_arguments(json)
                };
                *args = Some(parsed.clone());
                out.push(StreamEvent::ToolCallEnd(AssistantBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: parsed,
                    provider_signature: None,
                }));
            }
        }
    }

    fn read_usage(&mut self, usage: &Value, from_start: bool) {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64);
        if from_start {
            self.usage.input_tokens = field("input_tokens").unwrap_or(0);
            self.usage.output_tokens = field("output_tokens").unwrap_or(0);
            self.usage.cache_read_tokens = field("cache_read_input_tokens").unwrap_or(0);
            self.usage.cache_write_tokens = field("cache_creation_input_tokens").unwrap_or(0);
            return;
        }
        // Only fields present (not null) replace what `message_start` said.
        if let Some(v) = field("input_tokens") {
            self.usage.input_tokens = v;
        }
        if let Some(v) = field("output_tokens") {
            self.usage.output_tokens = v;
        }
        if let Some(v) = field("cache_read_input_tokens") {
            self.usage.cache_read_tokens = v;
        }
        if let Some(v) = field("cache_creation_input_tokens") {
            self.usage.cache_write_tokens = v;
        }
        if let Some(v) = usage
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.thinking_tokens = v;
        }
    }

    fn message_delta(&mut self, value: &Value) {
        if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
            let explanation = value
                .pointer("/delta/stop_details/explanation")
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty());
            match map_stop_reason(reason, explanation) {
                Ok(stop) => {
                    self.stop = Some(stop);
                    self.stop_error = None;
                }
                Err(error) => {
                    self.stop = Some(StopReason::Unknown);
                    self.stop_error = Some(error);
                }
            }
        }
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            self.read_usage(usage, false);
        }
    }

    fn message(&self) -> Message {
        let content = self
            .blocks
            .iter()
            .map(|(_, p)| match p {
                Partial::Text { text } => AssistantBlock::Text {
                    text: text.clone(),
                    signature: None,
                },
                Partial::Thinking {
                    text,
                    signature,
                    redacted,
                } => AssistantBlock::Thinking {
                    text: text.clone(),
                    signature: Some(signature.clone()).filter(|s| !s.is_empty()),
                    redacted: *redacted,
                },
                Partial::ToolCall {
                    id,
                    name,
                    json,
                    args,
                    ..
                } => AssistantBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone().unwrap_or_else(|| parse_tool_arguments(json)),
                    provider_signature: None,
                },
            })
            .collect();
        Message::Assistant {
            content,
            provider: self.provider,
            api: WireApi::AnthropicMessages,
            model: self.model.clone(),
            stop: self.stop.unwrap_or(StopReason::Unknown),
            aborted: false,
            failed: false,
            usage: self.usage,
            response_id: self.response_id.clone(),
        }
    }
}

impl StreamParser for AnthropicParser {
    fn feed(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, ClassifiedError> {
        let mut out = Vec::new();
        if self.terminal || event.data.trim().is_empty() {
            return Ok(out);
        }
        let value = parse_event_json(&event.data).map_err(|e| common::not_json(&e))?;
        let kind = event
            .event
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        match kind.as_str() {
            "message_start" => {
                self.started = true;
                if let Some(id) = value.pointer("/message/id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
                if let Some(usage) = value.pointer("/message/usage").filter(|u| u.is_object()) {
                    self.read_usage(usage, true);
                }
            }
            "content_block_start" => {
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("fallback") {
                    if !self.blocks.is_empty() {
                        return Err(common::classified(
                            ProviderErrorKind::Other,
                            None,
                            "Anthropic switched models after the reply had begun".into(),
                            ErrorClass::Retryable,
                        ));
                    }
                } else {
                    self.block_start(index, &block, &mut out);
                }
            }
            "content_block_delta" => {
                if let Some(delta) = value.get("delta") {
                    self.block_delta(index, delta, &mut out);
                }
            }
            "content_block_stop" => self.block_stop(index, &mut out),
            "message_delta" => self.message_delta(&value),
            "message_stop" => self.terminal = true,
            "error" => return Err(stream_error(&value)),
            other => {
                tracing::debug!(event = other, "anthropic: ignoring a stream event");
            }
        }
        Ok(out)
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ClassifiedError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        if !self.terminal {
            return Err(common::no_terminal_event("Anthropic"));
        }
        if let Some(error) = self.stop_error.take() {
            return Err(error);
        }
        if self.stop.is_none() {
            return Err(common::classified(
                ProviderErrorKind::Other,
                None,
                "the Anthropic stream ended without a stop reason".into(),
                ErrorClass::Retryable,
            ));
        }
        if let Some((_, Partial::ToolCall { name, .. })) = self
            .blocks
            .iter()
            .find(|(_, p)| matches!(p, Partial::ToolCall { args: None, .. }))
        {
            return Err(common::classified(
                ProviderErrorKind::Other,
                None,
                format!("the response completed with an unfinished tool call ({name})"),
                ErrorClass::Retryable,
            ));
        }
        Ok(vec![StreamEvent::Done {
            message: self.message(),
        }])
    }

    fn partial_message(&self) -> Message {
        let mut message = self.message();
        if let Message::Assistant { aborted, .. } = &mut message {
            *aborted = true;
        }
        message
    }
}

fn str_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Pi's `mapStopReason`: a refusal, a safety stop or an unknown reason is an error.
fn map_stop_reason(reason: &str, explanation: Option<&str>) -> Result<StopReason, ClassifiedError> {
    let fatal = |message: String| {
        common::classified(ProviderErrorKind::Other, None, message, ErrorClass::Fatal)
    };
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok(StopReason::EndTurn),
        "max_tokens" => Ok(StopReason::MaxTokens),
        "tool_use" => Ok(StopReason::ToolUse),
        "refusal" => Err(fatal(format!(
            "the model refused to complete the request{}",
            explanation.map(|e| format!(": {e}")).unwrap_or_default()
        ))),
        "sensitive" => Err(fatal("the provider stopped the reply: sensitive".into())),
        other => Err(fatal(format!(
            "the reply stopped for an unknown reason: {other}"
        ))),
    }
}

// =============================================================================================
// Errors
// =============================================================================================

/// `(type, message)` from an Anthropic error body (`{"type":"error","error":{…}}`).
fn error_fields(value: &Value) -> (Option<&str>, Option<&str>) {
    let error = value.get("error").unwrap_or(value);
    (
        error.get("type").and_then(Value::as_str),
        error.get("message").and_then(Value::as_str),
    )
}

/// Classifies a failed HTTP response.
pub fn classify_http_error(status: u16, headers: &HeaderMap, body: &str) -> ClassifiedError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let (code, message) = parsed.as_ref().map_or((None, None), error_fields);
    let message = message.or_else(|| (!body.trim().is_empty() && parsed.is_none()).then_some(body));
    let text = format!(
        "Anthropic API error (HTTP {status}): {}",
        common::error_detail(code, message)
    );
    let overflow =
        code == Some("request_too_large") || message.is_some_and(common::is_context_overflow);
    let classified = common::classify_status(status, text, overflow);
    common::honour_should_retry(classified, header_str(headers, "x-should-retry"))
}

/// An `error` event in the stream.
fn stream_error(value: &Value) -> ClassifiedError {
    let (code, message) = error_fields(value);
    let text = format!(
        "the Anthropic stream reported an error: {}",
        common::error_detail(code, message)
    );
    if message.is_some_and(common::is_context_overflow) || code == Some("request_too_large") {
        return common::classified(
            ProviderErrorKind::ContextOverflow,
            None,
            text,
            ErrorClass::ContextOverflow,
        );
    }
    match code {
        Some("overloaded_error") => common::classified(
            ProviderErrorKind::Overloaded,
            None,
            text,
            ErrorClass::Retryable,
        ),
        Some("rate_limit_error") => common::classified(
            ProviderErrorKind::RateLimited,
            None,
            text,
            ErrorClass::Retryable,
        ),
        Some("api_error" | "timeout_error") => {
            common::classified(ProviderErrorKind::Server, None, text, ErrorClass::Retryable)
        }
        _ => common::classified(ProviderErrorKind::Other, None, text, ErrorClass::Fatal),
    }
}
