//! Gemini through the Generative Language API.
//!
//! A Rust port of Pi's `packages/ai/src/api/google-generative-ai.ts` and `google-shared.ts` at
//! the pinned commit `1402973a` (MIT, Copyright Earendil Works), over plain HTTP and SSE (no
//! Google SDK).
//!
//! - **Request**: `POST <base>/models/<model>:streamGenerateContent?alt=sse` on
//!   `https://generativelanguage.googleapis.com/v1beta`, the key in `x-goog-api-key`
//!   ([`WireAdapter::credential_header`]). Body: `contents` (`user` and `model` turns),
//!   `systemInstruction`, `tools` (`functionDeclarations` with `parametersJsonSchema`) and
//!   `generationConfig` (`maxOutputTokens` for the orchestrators' cap, `thinkingConfig`).
//! - **Effort** (through the catalog): Gemini 3 and later take a thinking level
//!   (`MINIMAL`, `LOW`, `MEDIUM`, `HIGH`: the catalog's value for the rung, upper-cased); older
//!   models a thinking budget (Pi's table per level). A rung mapped to `off` sends a budget of
//!   0 to budget models and the lowest level the catalog offers to level models (they can't turn
//!   thinking off); an unsupported rung or value sends no level. `includeThoughts` is always on
//!   for reasoning models, so thought summaries and signatures come back.
//! - **Replay**: thought signatures (base64) are kept as the signature of the thinking or text part
//!   they came on, and sent back only to the same model, only when valid base64. A signature on a
//!   function call is kept as the tool call's `provider_signature`; the replay rules drop it for
//!   another model, as they drop its thinking, and it goes back on the call's part. Tool-call IDs
//!   are sanitised to `[A-Za-z0-9_-]`, at most 64 characters, and sent for Gemini 3 and later. Tool
//!   results of one turn share one `user` turn; images go inside the function response for Gemini 3
//!   and later, in a `user` turn of their own before that.
//! - **Stream**: each event is a `GenerateContentResponse`. Text and thought parts open blocks
//!   (a change of kind ends the previous one); a function call is a whole tool call (start,
//!   arguments, end at once; a missing or repeated ID gets a generated one). A stream without a
//!   `finishReason` is an error; any finish other than `STOP` or `MAX_TOKENS` is an error that
//!   isn't retried; an `error` object is reported.

use super::common::{self, EffortSetting};
use super::responses::{parse_event_json, short_hash};
use super::{
    AdapterError, ClassifiedError, CredentialHeader, CredentialKind, ErrorClass, HeaderMap,
    HttpRequest, ModelRequest, StreamEvent, StreamParser, WireAdapter, WireApi,
};
use crate::sse::SseEvent;
use crate::transcript::{AssistantBlock, Block, Message};
use qm_engine::{CallUsage, ProviderErrorKind, StopReason};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

/// The Generative Language API's default base URL.
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The Gemini adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn url(base_url: Option<&str>, model: &str) -> String {
        let base = base_url
            .filter(|b| !b.trim().is_empty())
            .unwrap_or(GEMINI_BASE_URL)
            .trim_end_matches('/');
        format!("{base}/models/{model}:streamGenerateContent?alt=sse")
    }
}

impl WireAdapter for GeminiAdapter {
    fn reported_usage(&self) -> qm_engine::ReportedUsage {
        qm_engine::ReportedUsage::NONE
            .with_thinking()
            .with_cache_read()
    }

    fn api(&self) -> WireApi {
        WireApi::GoogleGenerativeAi
    }

    fn build_request(&self, req: &ModelRequest<'_>) -> Result<HttpRequest, AdapterError> {
        let model = req.model.id.as_str();
        let mut body = Map::new();
        body.insert(
            "contents".into(),
            Value::Array(convert_messages(req.messages, model, req.model.image_input)),
        );
        if !req.system_prompt.is_empty() {
            body.insert(
                "systemInstruction".into(),
                json!({ "parts": [{ "text": req.system_prompt }], "role": "user" }),
            );
        }
        if !req.tools.is_empty() {
            let declarations: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parametersJsonSchema": t.input_schema,
                    })
                })
                .collect();
            body.insert(
                "tools".into(),
                json!([{ "functionDeclarations": declarations }]),
            );
        }
        let mut generation = Map::new();
        if let Some(cap) = req
            .max_output_tokens
            .filter(|_| self.can_send_output_cap(req))
        {
            generation.insert("maxOutputTokens".into(), json!(cap));
        }
        if let Some(thinking) = thinking_config(req) {
            generation.insert("thinkingConfig".into(), thinking);
        }
        if !generation.is_empty() {
            body.insert("generationConfig".into(), Value::Object(generation));
        }
        Ok(HttpRequest {
            url: Self::url(req.base_url, model),
            headers: vec![
                ("User-Agent".to_owned(), req.user_agent.to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ],
            body: Value::Object(body),
        })
    }

    fn parser(&self, req: &ModelRequest<'_>) -> Box<dyn StreamParser> {
        Box::new(GeminiParser::new(req.provider, req.model.id.clone()))
    }

    fn classify_error(&self, status: u16, _headers: &HeaderMap, body: &str) -> ClassifiedError {
        classify_http_error(status, body)
    }

    fn normalize_tool_call_id(&self, id: &str, _from_foreign_model: bool) -> String {
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

    fn can_send_output_cap(&self, _req: &ModelRequest<'_>) -> bool {
        true
    }

    fn credential_header(&self, credential: &CredentialKind) -> CredentialHeader {
        match credential {
            CredentialKind::ApiKey => CredentialHeader::Named("x-goog-api-key"),
            CredentialKind::SignInWithChatGpt | CredentialKind::Codex { .. } => {
                CredentialHeader::Bearer
            }
        }
    }
}

// =============================================================================================
// Model families
// =============================================================================================

/// The major version in `gemini-<n>…` (or `gemini-live-<n>…`).
fn gemini_major_version(model: &str) -> Option<u32> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"^gemini(?:-live)?-(\d+)").ok())
        .as_ref()?;
    re.captures(&model.to_ascii_lowercase())?
        .get(1)?
        .as_str()
        .parse()
        .ok()
}

/// Whether the API wants tool-call IDs on function calls and responses.
fn requires_tool_call_id(model: &str) -> bool {
    model.starts_with("claude-")
        || model.starts_with("gpt-oss-")
        || gemini_major_version(model).is_some_and(|v| v >= 3)
}

/// Whether images may go inside a function response.
fn multimodal_function_response(model: &str) -> bool {
    gemini_major_version(model).is_none_or(|v| v >= 3)
}

/// Whether the model takes a thinking level rather than a thinking budget.
pub fn uses_thinking_level(model: &str) -> bool {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    let id = model.to_ascii_lowercase();
    let matches = RE
        .get_or_init(|| {
            Regex::new(r"gemini-3(?:\.\d+)?-(?:pro|flash)|gemini-(?:[4-9]|[1-9]\d)|gemma-?4").ok()
        })
        .as_ref()
        .is_some_and(|re| re.is_match(&id));
    matches || id == "gemini-flash-latest" || id == "gemini-flash-lite-latest"
}

/// A provider value as Gemini's thinking level, if it is one.
fn thinking_level(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "minimal" => Some("MINIMAL"),
        "low" => Some("LOW"),
        "medium" => Some("MEDIUM"),
        "high" => Some("HIGH"),
        _ => None,
    }
}

/// Pi's thinking budgets for budget models (Gemini 2.5), else -1 (dynamic).
fn thinking_budget(model: &str, level: &str) -> i64 {
    let table: [i64; 4] = if model.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if model.contains("2.5-flash-lite") {
        [512, 2048, 8192, 24576]
    } else if model.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return -1;
    };
    match level {
        "MINIMAL" => table[0],
        "LOW" => table[1],
        "MEDIUM" => table[2],
        _ => table[3],
    }
}

/// `generationConfig.thinkingConfig` for the request's effort.
fn thinking_config(req: &ModelRequest<'_>) -> Option<Value> {
    if !req.model.reasoning {
        return None;
    }
    let model = req.model.id.as_str();
    let level_model = uses_thinking_level(model);
    let with_level = |level: &str| {
        if level_model {
            json!({ "includeThoughts": true, "thinkingLevel": level })
        } else {
            json!({ "includeThoughts": true, "thinkingBudget": thinking_budget(model, level) })
        }
    };
    Some(match common::effort_setting(req) {
        EffortSetting::Value(value) => match thinking_level(&value) {
            Some(level) => with_level(level),
            None => json!({ "includeThoughts": true }),
        },
        EffortSetting::Off if level_model => {
            match common::lowest_effort_value(req.provider, &req.model.id)
                .as_deref()
                .and_then(thinking_level)
            {
                Some(level) => json!({ "thinkingLevel": level }),
                None => json!({ "includeThoughts": true }),
            }
        }
        EffortSetting::Off => json!({ "thinkingBudget": 0 }),
        EffortSetting::Omit => json!({ "includeThoughts": true }),
    })
}

// =============================================================================================
// Request conversion
// =============================================================================================

/// Gemini wants thought signatures as base64.
fn valid_signature(signature: &str) -> bool {
    !signature.is_empty() && signature.len().is_multiple_of(4) && {
        let body = signature.trim_end_matches('=');
        signature.len() - body.len() <= 2
            && !body.is_empty()
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    }
}

fn inline_data(media_type: &str, bytes: &[u8]) -> Value {
    use base64::Engine as _;
    json!({
        "inlineData": {
            "mimeType": media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    })
}

/// One assistant message's parts (the replay rules already dropped another model's thinking
/// and signatures).
fn convert_assistant(content: &[AssistantBlock], model: &str) -> Vec<Value> {
    let mut parts = Vec::new();
    for block in content {
        match block {
            AssistantBlock::Text { text, signature } => {
                let signature = signature.as_deref().filter(|s| valid_signature(s));
                if text.trim().is_empty() && signature.is_none() {
                    continue;
                }
                let mut part = Map::new();
                part.insert("text".into(), json!(text));
                if let Some(signature) = signature {
                    part.insert("thoughtSignature".into(), json!(signature));
                }
                parts.push(Value::Object(part));
            }
            AssistantBlock::Thinking {
                text, signature, ..
            } => {
                let signature = signature.as_deref().filter(|s| valid_signature(s));
                if text.trim().is_empty() && signature.is_none() {
                    continue;
                }
                let mut part = Map::new();
                part.insert("thought".into(), json!(true));
                part.insert("text".into(), json!(text));
                if let Some(signature) = signature {
                    part.insert("thoughtSignature".into(), json!(signature));
                }
                parts.push(Value::Object(part));
            }
            AssistantBlock::ToolCall {
                id,
                name,
                args,
                provider_signature,
            } => {
                let mut call = Map::new();
                call.insert("name".into(), json!(name));
                call.insert("args".into(), common::args_object(args));
                if requires_tool_call_id(model) {
                    call.insert("id".into(), json!(id));
                }
                let mut part = Map::new();
                part.insert("functionCall".into(), Value::Object(call));
                if let Some(signature) =
                    provider_signature.as_deref().filter(|s| valid_signature(s))
                {
                    part.insert("thoughtSignature".into(), json!(signature));
                }
                parts.push(Value::Object(part));
            }
        }
    }
    parts
}

/// Our prepared transcript as Gemini `contents` (Pi's `convertMessages`).
fn convert_messages(messages: &[Message], model: &str, image_input: bool) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    for msg in messages {
        match msg {
            Message::User { content } => {
                let parts: Vec<Value> = content
                    .iter()
                    .map(|b| match b {
                        Block::Text { text } => json!({ "text": text }),
                        Block::Image { media_type, bytes } => inline_data(media_type, bytes),
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Message::Assistant { content, .. } => {
                let parts = convert_assistant(content, model);
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Message::ToolResult {
                call_id,
                name,
                content,
                is_error,
            } => {
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(text.as_str()),
                        Block::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<Value> = if image_input {
                    content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Image { media_type, bytes } => {
                                Some(inline_data(media_type, bytes))
                            }
                            Block::Text { .. } => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let value = if !text.trim().is_empty() {
                    text
                } else if images.is_empty() {
                    String::new()
                } else {
                    "(see attached image)".to_owned()
                };
                let multimodal = multimodal_function_response(model);
                let mut response = Map::new();
                response.insert("name".into(), json!(name));
                response.insert(
                    "response".into(),
                    if *is_error {
                        json!({ "error": value })
                    } else {
                        json!({ "output": value })
                    },
                );
                if !images.is_empty() && multimodal {
                    response.insert("parts".into(), Value::Array(images.clone()));
                }
                if requires_tool_call_id(model) {
                    response.insert("id".into(), json!(call_id));
                }
                let part = json!({ "functionResponse": response });
                let merged = contents.last_mut().is_some_and(|last| {
                    let is_responses = last.get("role").and_then(Value::as_str) == Some("user")
                        && last
                            .get("parts")
                            .and_then(Value::as_array)
                            .is_some_and(|p| p.iter().any(|p| p.get("functionResponse").is_some()));
                    if is_responses
                        && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
                    {
                        parts.push(part.clone());
                        true
                    } else {
                        false
                    }
                });
                if !merged {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }
                if !images.is_empty() && !multimodal {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(images);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }
    contents
}

// =============================================================================================
// Stream parsing
// =============================================================================================

#[derive(Debug)]
enum Partial {
    Text {
        text: String,
        signature: Option<String>,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
        /// The function call's thought signature.
        signature: Option<String>,
    },
}

/// Parses one Gemini stream (Pi's `stream`).
#[derive(Debug)]
pub struct GeminiParser {
    provider: Provider,
    model: ModelId,
    blocks: Vec<Partial>,
    /// The open text or thinking block.
    current: Option<usize>,
    generated_ids: u32,
    response_id: Option<String>,
    usage: CallUsage,
    stop: Option<StopReason>,
    stop_error: Option<ClassifiedError>,
    finished: bool,
}

impl GeminiParser {
    pub fn new(provider: Provider, model: ModelId) -> Self {
        Self {
            provider,
            model,
            blocks: Vec::new(),
            current: None,
            generated_ids: 0,
            response_id: None,
            usage: CallUsage::default(),
            stop: None,
            stop_error: None,
            finished: false,
        }
    }

    fn end_current(&mut self, out: &mut Vec<StreamEvent>) {
        let Some(i) = self.current.take() else {
            return;
        };
        match &self.blocks[i] {
            Partial::Text { .. } => out.push(StreamEvent::TextEnd),
            Partial::Thinking { text, signature } => {
                out.push(StreamEvent::ThinkingEnd(AssistantBlock::Thinking {
                    text: text.clone(),
                    signature: signature.clone(),
                    redacted: false,
                }));
            }
            Partial::ToolCall { .. } => {}
        }
    }

    fn text_part(&mut self, part: &Value, piece: &str, out: &mut Vec<StreamEvent>) {
        let thought = part.get("thought").and_then(Value::as_bool) == Some(true);
        let signature = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let same_kind = self.current.is_some_and(|i| {
            matches!(
                (&self.blocks[i], thought),
                (Partial::Thinking { .. }, true) | (Partial::Text { .. }, false)
            )
        });
        if !same_kind {
            self.end_current(out);
            self.blocks.push(if thought {
                Partial::Thinking {
                    text: String::new(),
                    signature: None,
                }
            } else {
                Partial::Text {
                    text: String::new(),
                    signature: None,
                }
            });
            self.current = Some(self.blocks.len() - 1);
        }
        let Some(i) = self.current else {
            return;
        };
        match &mut self.blocks[i] {
            Partial::Thinking {
                text,
                signature: sig,
            } => {
                text.push_str(piece);
                if let Some(s) = signature {
                    *sig = Some(s.to_owned());
                }
                out.push(StreamEvent::ThinkingDelta(piece.to_owned()));
            }
            Partial::Text {
                text,
                signature: sig,
            } => {
                text.push_str(piece);
                if let Some(s) = signature {
                    *sig = Some(s.to_owned());
                }
                out.push(StreamEvent::TextDelta(piece.to_owned()));
            }
            Partial::ToolCall { .. } => {}
        }
    }

    fn function_call(&mut self, part: &Value, call: &Value, out: &mut Vec<StreamEvent>) {
        self.end_current(out);
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let provided = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let duplicate = provided.is_some_and(|p| {
            self.blocks
                .iter()
                .any(|b| matches!(b, Partial::ToolCall { id, .. } if id == p))
        });
        let id = match provided {
            Some(id) if !duplicate => id.to_owned(),
            _ => {
                self.generated_ids += 1;
                let seed = self
                    .response_id
                    .as_deref()
                    .map_or_else(|| uuid::Uuid::new_v4().simple().to_string(), short_hash);
                format!("{name}_{seed}_{}", self.generated_ids)
            }
        };
        let args = call
            .get("args")
            .filter(|a| a.is_object())
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let signature = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        out.push(StreamEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
        });
        out.push(StreamEvent::ToolCallArgsDelta(args.to_string()));
        out.push(StreamEvent::ToolCallEnd(AssistantBlock::ToolCall {
            id: id.clone(),
            name: name.clone(),
            args: args.clone(),
            provider_signature: signature.clone(),
        }));
        self.blocks.push(Partial::ToolCall {
            id,
            name,
            args,
            signature,
        });
    }

    fn read_usage(&mut self, usage: &Value) {
        let n = |f: &str| usage.get(f).and_then(Value::as_u64).unwrap_or(0);
        let cached = n("cachedContentTokenCount");
        let thoughts = n("thoughtsTokenCount");
        self.usage = CallUsage {
            input_tokens: n("promptTokenCount").saturating_sub(cached),
            output_tokens: n("candidatesTokenCount") + thoughts,
            thinking_tokens: thoughts,
            cache_read_tokens: cached,
            cache_write_tokens: 0,
        };
    }

    fn has_tool_calls(&self) -> bool {
        self.blocks
            .iter()
            .any(|b| matches!(b, Partial::ToolCall { .. }))
    }

    fn message(&self) -> Message {
        let content = self
            .blocks
            .iter()
            .map(|b| match b {
                Partial::Text { text, signature } => AssistantBlock::Text {
                    text: text.clone(),
                    signature: signature.clone(),
                },
                Partial::Thinking { text, signature } => AssistantBlock::Thinking {
                    text: text.clone(),
                    signature: signature.clone(),
                    redacted: false,
                },
                Partial::ToolCall {
                    id,
                    name,
                    args,
                    signature,
                } => AssistantBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    provider_signature: signature.clone(),
                },
            })
            .collect();
        Message::Assistant {
            content,
            provider: self.provider,
            api: WireApi::GoogleGenerativeAi,
            model: self.model.clone(),
            stop: self.stop.unwrap_or(StopReason::Unknown),
            aborted: false,
            failed: false,
            usage: self.usage,
            response_id: self.response_id.clone(),
        }
    }
}

impl StreamParser for GeminiParser {
    fn feed(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, ClassifiedError> {
        let mut out = Vec::new();
        let data = event.data.trim();
        if self.finished || data.is_empty() || data == "[DONE]" {
            return Ok(out);
        }
        let chunk = parse_event_json(data).map_err(|e| common::not_json(&e))?;
        let chunk = match chunk {
            Value::Array(mut items) if !items.is_empty() => items.remove(0),
            other => other,
        };
        if let Some(error) = chunk.get("error").filter(|e| e.is_object()) {
            return Err(stream_error(error));
        }
        if self.response_id.is_none()
            && let Some(id) = chunk.get("responseId").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.response_id = Some(id.to_owned());
        }
        let candidate = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first());
        if let Some(parts) = candidate
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(piece) = part.get("text").and_then(Value::as_str) {
                    self.text_part(part, piece, &mut out);
                }
                if let Some(call) = part.get("functionCall").filter(|c| c.is_object()) {
                    self.function_call(part, call, &mut out);
                }
            }
        }
        if let Some(reason) = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
        {
            match reason {
                "STOP" => {
                    self.stop = Some(if self.has_tool_calls() {
                        StopReason::ToolUse
                    } else {
                        StopReason::EndTurn
                    });
                    self.stop_error = None;
                }
                "MAX_TOKENS" => {
                    self.stop = Some(StopReason::MaxTokens);
                    self.stop_error = None;
                }
                other => {
                    self.stop = Some(StopReason::Unknown);
                    self.stop_error = Some(common::classified(
                        ProviderErrorKind::Other,
                        None,
                        format!("Gemini stopped the reply: {other}"),
                        ErrorClass::Fatal,
                    ));
                }
            }
        }
        if let Some(usage) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
            self.read_usage(usage);
        }
        Ok(out)
    }

    fn is_terminal(&self) -> bool {
        // Usage may follow the finish reason: read to the end of the body.
        false
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ClassifiedError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut out = Vec::new();
        self.end_current(&mut out);
        if let Some(error) = self.stop_error.take() {
            return Err(error);
        }
        if self.stop.is_none() {
            return Err(common::no_terminal_event("Gemini"));
        }
        out.push(StreamEvent::Done {
            message: self.message(),
        });
        Ok(out)
    }

    fn partial_message(&self) -> Message {
        let mut message = self.message();
        if let Message::Assistant { aborted, .. } = &mut message {
            *aborted = true;
        }
        message
    }
}

// =============================================================================================
// Errors
// =============================================================================================

/// `(status, message, reason)` from a Google error object.
fn error_fields(error: &Value) -> (Option<&str>, Option<&str>, Option<&str>) {
    let reason = error
        .get("details")
        .and_then(Value::as_array)
        .and_then(|d| {
            d.iter()
                .find_map(|d| d.get("reason").and_then(Value::as_str))
        });
    (
        error.get("status").and_then(Value::as_str),
        error.get("message").and_then(Value::as_str),
        reason,
    )
}

/// Classifies a failed HTTP response (the body may be one error object or an array of them).
pub fn classify_http_error(status: u16, body: &str) -> ClassifiedError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let parsed = match parsed {
        Some(Value::Array(mut items)) if !items.is_empty() => Some(items.remove(0)),
        other => other,
    };
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let (code, message, reason) = error.map_or((None, None, None), error_fields);
    let message = message.or_else(|| (!body.trim().is_empty() && parsed.is_none()).then_some(body));
    let text = format!(
        "Gemini API error (HTTP {status}): {}",
        common::error_detail(code, message)
    );
    let message_lc = message.unwrap_or_default().to_ascii_lowercase();
    if reason == Some("API_KEY_INVALID") || message_lc.contains("api key not valid") {
        return common::classified(
            ProviderErrorKind::Auth,
            Some(status),
            text,
            ErrorClass::Unauthorized,
        );
    }
    if status == 429 && message_lc.contains("quota") && message_lc.contains("billing") {
        return common::classified(
            ProviderErrorKind::RateLimited,
            Some(status),
            text,
            ErrorClass::AccountLimit {
                window: None,
                resets_at: None,
            },
        );
    }
    let overflow = message.is_some_and(common::is_context_overflow);
    common::classify_status(status, text, overflow)
}

/// An `error` object in the stream.
fn stream_error(error: &Value) -> ClassifiedError {
    let (code, message, _) = error_fields(error);
    let text = format!(
        "the Gemini stream reported an error: {}",
        common::error_detail(code, message)
    );
    if message.is_some_and(common::is_context_overflow) {
        return common::classified(
            ProviderErrorKind::ContextOverflow,
            None,
            text,
            ErrorClass::ContextOverflow,
        );
    }
    match code {
        Some("UNAVAILABLE" | "INTERNAL" | "RESOURCE_EXHAUSTED" | "DEADLINE_EXCEEDED") => {
            common::classified(ProviderErrorKind::Server, None, text, ErrorClass::Retryable)
        }
        _ => common::classified(ProviderErrorKind::Other, None, text, ErrorClass::Fatal),
    }
}
