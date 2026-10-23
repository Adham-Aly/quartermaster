//! The OpenAI Responses API, in three flavours.
//!
//! A Rust port of Pi's `packages/ai/src/api/openai-responses.ts`, `openai-responses-shared.ts`
//! and `openai-codex-responses.ts` at the pinned commit `1402973a` (MIT, Copyright Earendil
//! Works), over SSE only (no WebSocket transport, no zstd request compression).
//!
//! - **Flavour A, [`ResponsesAdapter::platform`]**: `POST <base>/responses` on
//!   `https://api.openai.com/v1`, for Sign in with ChatGPT tokens and `sk-` API keys. The system
//!   prompt is the first input item (`developer` for reasoning models, else `system`). Sign in
//!   with ChatGPT rejects `max_output_tokens`, `temperature`, `prompt_cache_retention` and
//!   `configuration_update` items, so they are never sent with it; an API key gets
//!   `max_output_tokens` (at least 16) when the orchestrators' cap applies.
//! - **Flavour B, [`ResponsesAdapter::codex`]**: `POST https://chatgpt.com/backend-api/codex/
//!   responses` for the Codex login, with `chatgpt-account-id`, `originator` and `OpenAI-Beta`
//!   headers, the system prompt as `instructions`, and `text.verbosity`, `tool_choice` and
//!   `parallel_tool_calls` in the body. Its `response.done` event is read as `response.completed`.
//! - **Flavour C, [`ResponsesAdapter::xai`]**: xAI's Responses API, `POST <base>/responses` on
//!   `https://api.x.ai/v1`, for xAI API keys and Grok subscription logins alike (Pi's xAI
//!   provider at the pin uses the same API). It is flavour A with xAI's own rules: the effort is
//!   the catalog's provider value for the model (`reasoning.effort`), and a rung the model
//!   doesn't support sends no `reasoning` object; the output cap is sent with either credential;
//!   tool-call IDs from another model are sanitised whole (Pi splits `call_id|item_id` only for
//!   OpenAI's own providers); errors name xAI.
//!
//! Compatibility flags come from the catalog's corrections, as for the other adapters; an absent
//! flag keeps OpenAI's own behaviour ([`ResponsesCompat`]).
//!
//! Replay: reasoning items are kept whole (JSON) as the thinking block's signature and sent back
//! as input items to the same model; an `encrypted_content` missing from `output_item.done` is
//! backfilled from `response.completed`. Text keeps its output message's ID (and phase) as its
//! signature. Tool-call IDs are `call_id|item_id`.

use super::common::{self, EffortSetting};
use super::{
    AdapterError, ClassifiedError, CredentialKind, ErrorClass, HeaderMap, HttpRequest,
    ModelRequest, StreamEvent, StreamParser, WireAdapter, WireApi,
};
use crate::model::responses_effort;
use crate::sse::SseEvent;
use crate::transcript::{AssistantBlock, Block, Message};
use jiff::{SignedDuration, Timestamp};
use qm_engine::{CallUsage, ProviderError, ProviderErrorKind, StopReason, UsageWindow};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The Responses API's default base URL (flavour A).
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// xAI's base URL (flavour C).
pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";
/// The ChatGPT backend's default base URL (flavour B).
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
/// OpenAI rejects `max_output_tokens` below this.
pub const MIN_OUTPUT_TOKENS: u32 = 16;
/// The longest prompt-cache key OpenAI accepts.
pub const PROMPT_CACHE_KEY_MAX: usize = 64;
/// The Codex backend's fallback instructions when the system prompt is empty.
const CODEX_DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";
/// Usage-limit error codes: account limits, never retried.
const USAGE_LIMIT_CODES: &[&str] = &[
    "usage_limit_reached",
    "subscription_sharing_usage_limit_exceeded",
    "insufficient_quota",
    "usage_not_included",
];

/// Which flavour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavour {
    Platform,
    Codex,
    Xai,
}

impl Flavour {
    /// How errors name the provider.
    const fn vendor(self) -> &'static str {
        match self {
            Self::Platform | Self::Codex => "OpenAI",
            Self::Xai => "xAI",
        }
    }
}

/// The Responses adapter's compatibility flags for one model, from the catalog's corrections
/// (`compat`). A flag the catalog doesn't set keeps OpenAI's own behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponsesCompat {
    /// `supports_developer_role` (default on): a reasoning model's system prompt goes as a
    /// `developer` message, else as `system`.
    pub developer_role: bool,
    /// `supports_max_output_tokens` (default on): the output cap may be sent as
    /// `max_output_tokens`.
    pub max_output_tokens: bool,
    /// `always_include_encrypted_reasoning` (default off): ask for the encrypted reasoning even
    /// when no effort is sent (xAI's reasoning models always reason; Pi asks for it the same way).
    pub always_include_encrypted_reasoning: bool,
}

impl ResponsesCompat {
    /// The flags for `req`'s provider and model.
    pub fn for_request(req: &ModelRequest<'_>) -> Self {
        let flags = common::compat_flags(req.provider, &req.model.id);
        let flag = |name: &str, default: bool| flags.get(name).copied().unwrap_or(default);
        Self {
            developer_role: flag("supports_developer_role", true),
            max_output_tokens: flag("supports_max_output_tokens", true),
            always_include_encrypted_reasoning: flag("always_include_encrypted_reasoning", false),
        }
    }
}

/// The Responses adapter.
#[derive(Clone, Copy, Debug)]
pub struct ResponsesAdapter {
    flavour: Flavour,
}

impl ResponsesAdapter {
    /// Flavour A: `api.openai.com/v1/responses` (Sign in with ChatGPT, API keys).
    pub const fn platform() -> Self {
        Self {
            flavour: Flavour::Platform,
        }
    }

    /// Flavour B: the ChatGPT backend (the Codex login).
    pub const fn codex() -> Self {
        Self {
            flavour: Flavour::Codex,
        }
    }

    /// Flavour C: xAI's Responses API (xAI API keys and Grok subscription logins).
    pub const fn xai() -> Self {
        Self {
            flavour: Flavour::Xai,
        }
    }

    /// Flavours A and C: `reasoning` and `include` for a reasoning model.
    fn insert_reasoning(
        self,
        req: &ModelRequest<'_>,
        compat: ResponsesCompat,
        body: &mut Map<String, Value>,
    ) {
        let effort = match self.flavour {
            // xAI: the catalog's value for this model's rung, if it has one.
            Flavour::Xai => common::effort_setting(req),
            // OpenAI: our ladder as the Responses API names it.
            Flavour::Platform | Flavour::Codex => {
                EffortSetting::Value(responses_effort(req.effort).to_owned())
            }
        };
        match effort {
            EffortSetting::Value(effort) => {
                body.insert(
                    "reasoning".into(),
                    json!({ "effort": effort, "summary": "auto" }),
                );
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
            EffortSetting::Off => {
                body.insert("reasoning".into(), json!({ "effort": "none" }));
            }
            EffortSetting::Omit => {}
        }
        if compat.always_include_encrypted_reasoning {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }

    fn url(self, base_url: Option<&str>) -> String {
        match self.flavour {
            Flavour::Platform | Flavour::Xai => {
                let default = if self.flavour == Flavour::Xai {
                    XAI_BASE_URL
                } else {
                    OPENAI_BASE_URL
                };
                let base = base_url
                    .filter(|b| !b.trim().is_empty())
                    .unwrap_or(default)
                    .trim_end_matches('/');
                if base.ends_with("/responses") {
                    base.to_owned()
                } else {
                    format!("{base}/responses")
                }
            }
            Flavour::Codex => {
                let base = base_url
                    .filter(|b| !b.trim().is_empty())
                    .unwrap_or(CODEX_BASE_URL)
                    .trim_end_matches('/');
                if base.ends_with("/codex/responses") {
                    base.to_owned()
                } else if base.ends_with("/codex") {
                    format!("{base}/responses")
                } else {
                    format!("{base}/codex/responses")
                }
            }
        }
    }
}

impl WireAdapter for ResponsesAdapter {
    fn reported_usage(&self) -> qm_engine::ReportedUsage {
        qm_engine::ReportedUsage::NONE
            .with_thinking()
            .with_cache_read()
    }

    fn api(&self) -> WireApi {
        match self.flavour {
            Flavour::Platform | Flavour::Xai => WireApi::OpenAiResponses,
            Flavour::Codex => WireApi::OpenAiCodexResponses,
        }
    }

    fn build_request(&self, req: &ModelRequest<'_>) -> Result<HttpRequest, AdapterError> {
        let cache_key = clamp_cache_key(req.session_id);
        let mut headers = vec![
            ("User-Agent".to_owned(), req.user_agent.to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model.id.as_str()));
        match self.flavour {
            Flavour::Platform | Flavour::Xai => {
                if matches!(req.credential, CredentialKind::Codex { .. }) {
                    return Err(AdapterError::Invalid(
                        "a Codex-login token goes to the ChatGPT backend, not the Responses API"
                            .into(),
                    ));
                }
                let compat = ResponsesCompat::for_request(req);
                headers.push(("session_id".into(), req.session_id.to_owned()));
                headers.push(("x-client-request-id".into(), req.session_id.to_owned()));
                let system_role = if req.model.reasoning && compat.developer_role {
                    "developer"
                } else {
                    "system"
                };
                body.insert(
                    "input".into(),
                    Value::Array(convert_messages(self.api(), req, Some(system_role))),
                );
                body.insert("stream".into(), json!(true));
                body.insert("prompt_cache_key".into(), json!(cache_key));
                body.insert("store".into(), json!(false));
                if let Some(cap) = req.max_output_tokens
                    && self.can_send_output_cap(req)
                {
                    body.insert(
                        "max_output_tokens".into(),
                        json!(cap.max(MIN_OUTPUT_TOKENS)),
                    );
                }
                if !req.tools.is_empty() {
                    body.insert("tools".into(), convert_tools(req, false));
                }
                if req.model.reasoning {
                    self.insert_reasoning(req, compat, &mut body);
                }
            }
            Flavour::Codex => {
                let CredentialKind::Codex { account_id } = req.credential else {
                    return Err(AdapterError::Invalid(
                        "the ChatGPT backend needs a Codex-login token".into(),
                    ));
                };
                headers.push(("originator".into(), "quartermaster".into()));
                headers.push(("chatgpt-account-id".into(), account_id.clone()));
                headers.push(("OpenAI-Beta".into(), "responses=experimental".into()));
                headers.push(("session-id".into(), cache_key.clone()));
                headers.push(("x-client-request-id".into(), cache_key.clone()));
                body.insert("store".into(), json!(false));
                body.insert("stream".into(), json!(true));
                let instructions = if req.system_prompt.is_empty() {
                    CODEX_DEFAULT_INSTRUCTIONS
                } else {
                    req.system_prompt
                };
                body.insert("instructions".into(), json!(instructions));
                body.insert(
                    "input".into(),
                    Value::Array(convert_messages(self.api(), req, None)),
                );
                body.insert("text".into(), json!({ "verbosity": "low" }));
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
                body.insert("prompt_cache_key".into(), json!(cache_key));
                body.insert("tool_choice".into(), json!("auto"));
                body.insert("parallel_tool_calls".into(), json!(true));
                if !req.tools.is_empty() {
                    body.insert("tools".into(), convert_tools(req, true));
                }
                if req.model.reasoning {
                    body.insert(
                        "reasoning".into(),
                        json!({ "effort": responses_effort(req.effort), "summary": "auto" }),
                    );
                }
            }
        }
        Ok(HttpRequest {
            url: self.url(req.base_url),
            headers,
            body: Value::Object(body),
        })
    }

    fn parser(&self, req: &ModelRequest<'_>) -> Box<dyn StreamParser> {
        Box::new(ResponsesParser::new(
            req.provider,
            self.api(),
            req.model.id.clone(),
        ))
    }

    fn classify_error(&self, status: u16, headers: &HeaderMap, body: &str) -> ClassifiedError {
        classify_http_error(
            self.flavour.vendor(),
            status,
            headers,
            body,
            Timestamp::now(),
        )
    }

    fn normalize_tool_call_id(&self, id: &str, from_foreign_model: bool) -> String {
        match self.flavour {
            Flavour::Platform | Flavour::Codex => normalize_tool_call_id(id, from_foreign_model),
            // Pi splits `call_id|item_id` only for OpenAI's own providers: another model's ID
            // goes to xAI sanitised whole, as a call ID without an item ID.
            Flavour::Xai => normalize_id_part(id),
        }
    }

    fn can_send_output_cap(&self, req: &ModelRequest<'_>) -> bool {
        match self.flavour {
            Flavour::Platform => {
                *req.credential == CredentialKind::ApiKey
                    && ResponsesCompat::for_request(req).max_output_tokens
            }
            Flavour::Codex => false,
            Flavour::Xai => ResponsesCompat::for_request(req).max_output_tokens,
        }
    }

    fn usage_windows(&self, headers: &HeaderMap, now: Timestamp) -> Vec<UsageWindow> {
        match self.flavour {
            Flavour::Platform | Flavour::Codex => codex_usage_windows(headers, now),
            Flavour::Xai => Vec::new(),
        }
    }
}

fn clamp_cache_key(key: &str) -> String {
    key.chars().take(PROMPT_CACHE_KEY_MAX).collect()
}

// =============================================================================================
// Request conversion
// =============================================================================================

/// The `tools` array. The Codex backend gets `strict: null` (as Pi sends it); the platform API
/// gets no `strict` field.
fn convert_tools(req: &ModelRequest<'_>, codex: bool) -> Value {
    Value::Array(
        req.tools
            .iter()
            .map(|t| {
                let mut tool = Map::new();
                tool.insert("type".into(), json!("function"));
                tool.insert("name".into(), json!(t.name));
                tool.insert("description".into(), json!(t.description));
                tool.insert("parameters".into(), t.input_schema.clone());
                if codex {
                    tool.insert("strict".into(), Value::Null);
                }
                Value::Object(tool)
            })
            .collect(),
    )
}

/// Our prepared transcript as Responses input items (Pi's `convertResponsesMessages`), with the
/// system prompt first as a message of `system_role` when one is given (the Codex backend takes
/// it as `instructions` instead).
fn convert_messages(api: WireApi, req: &ModelRequest<'_>, system_role: Option<&str>) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(role) = system_role
        && !req.system_prompt.is_empty()
    {
        items.push(json!({ "role": role, "content": req.system_prompt }));
    }
    for (msg_index, msg) in req.messages.iter().enumerate() {
        match msg {
            Message::User { content } => {
                let content: Vec<Value> = content
                    .iter()
                    .map(|block| match block {
                        Block::Text { text } => json!({ "type": "input_text", "text": text }),
                        Block::Image { media_type, bytes } => json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": data_url(media_type, bytes),
                        }),
                    })
                    .collect();
                if !content.is_empty() {
                    items.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant {
                content,
                provider,
                api: msg_api,
                model,
                ..
            } => {
                let same_provider_and_api = *provider == req.provider && *msg_api == api;
                let different_model = same_provider_and_api && model != &req.model.id;
                convert_assistant(content, msg_index, different_model, &mut items);
            }
            Message::ToolResult {
                call_id, content, ..
            } => {
                let call_id = call_id.split_once('|').map_or(call_id.as_str(), |(c, _)| c);
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_result_output(content, req.model.image_input),
                }));
            }
        }
    }
    items
}

/// One assistant message's blocks as output items: reasoning items replayed whole, text as
/// output messages with their IDs, tool calls as `function_call` items.
fn convert_assistant(
    content: &[AssistantBlock],
    msg_index: usize,
    different_model: bool,
    items: &mut Vec<Value>,
) {
    let mut text_index = 0_usize;
    for block in content {
        match block {
            AssistantBlock::Thinking { signature, .. } => {
                if let Some(sig) = signature
                    && let Ok(item) = serde_json::from_str::<Value>(sig)
                {
                    items.push(item);
                }
            }
            AssistantBlock::Text { text, signature } => {
                let parsed = signature.as_deref().and_then(parse_text_signature);
                let fallback = if text_index == 0 {
                    format!("msg_qm_{msg_index}")
                } else {
                    format!("msg_qm_{msg_index}_{text_index}")
                };
                text_index += 1;
                let (id, phase) = match parsed {
                    Some((id, phase)) if id.len() > 64 => {
                        (format!("msg_{}", short_hash(&id)), phase)
                    }
                    Some((id, phase)) => (id, phase),
                    None => (fallback, None),
                };
                let mut item = Map::new();
                item.insert("type".into(), json!("message"));
                item.insert("role".into(), json!("assistant"));
                item.insert(
                    "content".into(),
                    json!([{ "type": "output_text", "text": text, "annotations": [] }]),
                );
                item.insert("status".into(), json!("completed"));
                item.insert("id".into(), json!(id));
                if let Some(phase) = phase {
                    item.insert("phase".into(), json!(phase));
                }
                items.push(Value::Object(item));
            }
            AssistantBlock::ToolCall { id, name, args, .. } => {
                let (call_id, item_id) = match id.split_once('|') {
                    Some((c, i)) => (c, Some(i)),
                    None => (id.as_str(), None),
                };
                // A different model's item IDs were paired with its own reasoning items; omit
                // them so the API doesn't try to pair them again.
                let item_id = item_id.filter(|i| !different_model && i.starts_with("fc_"));
                let arguments = match args {
                    Value::String(raw) => raw.clone(),
                    other => other.to_string(),
                };
                let mut item = Map::new();
                item.insert("type".into(), json!("function_call"));
                if let Some(item_id) = item_id {
                    item.insert("id".into(), json!(item_id));
                }
                item.insert("call_id".into(), json!(call_id));
                item.insert("name".into(), json!(name));
                item.insert("arguments".into(), json!(arguments));
                items.push(Value::Object(item));
            }
        }
    }
}

fn tool_result_output(content: &[Block], image_input: bool) -> Value {
    let text = content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            Block::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<(&str, &[u8])> = content
        .iter()
        .filter_map(|b| match b {
            Block::Image { media_type, bytes } => Some((media_type.as_str(), bytes.as_slice())),
            Block::Text { .. } => None,
        })
        .collect();
    let has_text = !text.trim().is_empty();
    if images.is_empty() || !image_input {
        let text = if has_text {
            text
        } else if images.is_empty() {
            "(no tool output)".to_owned()
        } else {
            "(see attached image)".to_owned()
        };
        return json!(text);
    }
    let mut parts = Vec::new();
    if has_text {
        parts.push(json!({ "type": "input_text", "text": text }));
    }
    for (media_type, bytes) in images {
        parts.push(json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": data_url(media_type, bytes),
        }));
    }
    Value::Array(parts)
}

pub(crate) fn data_url(media_type: &str, bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// A text signature: `{"v":1,"id":…,"phase":…}`, or a bare ID (older entries).
fn parse_text_signature(signature: &str) -> Option<(String, Option<String>)> {
    if signature.is_empty() {
        return None;
    }
    if signature.starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(signature)
        && v.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = v.get("id").and_then(Value::as_str)
    {
        let phase = v
            .get("phase")
            .and_then(Value::as_str)
            .filter(|p| matches!(*p, "commentary" | "final_answer"))
            .map(str::to_owned);
        return Some((id.to_owned(), phase));
    }
    Some((signature.to_owned(), None))
}

fn text_signature(id: &str, phase: Option<&str>) -> String {
    let mut sig = Map::new();
    sig.insert("v".into(), json!(1));
    sig.insert("id".into(), json!(id));
    if let Some(phase) = phase {
        sig.insert("phase".into(), json!(phase));
    }
    Value::Object(sig).to_string()
}

// =============================================================================================
// Tool-call IDs
// =============================================================================================

/// Pi's rule: each part of `call_id|item_id` sanitised to `[A-Za-z0-9_-]`, at most 64
/// characters, trailing `_` removed; a foreign item ID becomes `fc_<short hash>`; an item ID
/// not starting with `fc_` gets it.
pub fn normalize_tool_call_id(id: &str, from_foreign_model: bool) -> String {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return normalize_id_part(id);
    };
    // Pi splits on every `|` and keeps the first two parts.
    let item_id = item_id.split('|').next().unwrap_or_default();
    let call_id = normalize_id_part(call_id);
    let mut item_id = if from_foreign_model {
        let hashed = format!("fc_{}", short_hash(item_id));
        hashed.chars().take(64).collect()
    } else {
        normalize_id_part(item_id)
    };
    if !item_id.starts_with("fc_") {
        item_id = normalize_id_part(&format!("fc_{item_id}"));
    }
    format!("{call_id}|{item_id}")
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    sanitized.trim_end_matches('_').to_owned()
}

/// Pi's `shortHash` (`packages/ai/src/utils/hash.ts`), bit for bit: two 32-bit multiplicative
/// hashes over the UTF-16 code units, printed in base 36.
pub fn short_hash(text: &str) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for unit in text.encode_utf16() {
        let ch = u32::from(unit);
        h1 = (h1 ^ ch).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ ch).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", radix36(h2), radix36(h1))
}

fn radix36(mut n: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

// =============================================================================================
// Stream parsing
// =============================================================================================

#[derive(Debug)]
enum Slot {
    Thinking(usize),
    Text(usize),
    ToolCall(usize),
}

#[derive(Debug)]
enum Partial {
    Thinking {
        text: String,
        signature: Option<String>,
        item_id: Option<String>,
    },
    Text {
        text: String,
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        /// The streamed argument JSON; `None` once `output_item.done` finished the call.
        partial_json: Option<String>,
        args: Value,
    },
}

/// Parses one Responses stream (Pi's `processResponsesStream`, plus the Codex event mapping).
#[derive(Debug)]
pub struct ResponsesParser {
    provider: Provider,
    api: WireApi,
    model: ModelId,
    slots: BTreeMap<u64, Slot>,
    content: Vec<Partial>,
    response_id: Option<String>,
    usage: CallUsage,
    stop: Option<StopReason>,
    terminal: bool,
    finished: bool,
}

impl ResponsesParser {
    pub fn new(provider: Provider, api: WireApi, model: ModelId) -> Self {
        Self {
            provider,
            api,
            model,
            slots: BTreeMap::new(),
            content: Vec::new(),
            response_id: None,
            usage: CallUsage::default(),
            stop: None,
            terminal: false,
            finished: false,
        }
    }

    fn create_slot(&mut self, output_index: u64, item: &Value, out: &mut Vec<StreamEvent>) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let slot = match kind {
            "reasoning" => {
                self.content.push(Partial::Thinking {
                    text: String::new(),
                    signature: None,
                    item_id: None,
                });
                Slot::Thinking(self.content.len() - 1)
            }
            "message" => {
                self.content.push(Partial::Text {
                    text: String::new(),
                    signature: None,
                });
                Slot::Text(self.content.len() - 1)
            }
            "function_call" => {
                let call_id = str_field(item, "call_id");
                let item_id = str_field(item, "id");
                let name = str_field(item, "name");
                let id = format!("{call_id}|{item_id}");
                self.content.push(Partial::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    partial_json: Some(str_field(item, "arguments")),
                    args: Value::Object(Map::new()),
                });
                out.push(StreamEvent::ToolCallStart { id, name });
                Slot::ToolCall(self.content.len() - 1)
            }
            other => {
                tracing::debug!(item_type = other, "responses: ignoring an output item type");
                return;
            }
        };
        self.slots.insert(output_index, slot);
    }

    fn thinking_index(&self, output_index: u64) -> Option<usize> {
        match self.slots.get(&output_index) {
            Some(Slot::Thinking(i)) => Some(*i),
            _ => None,
        }
    }

    fn text_index(&self, output_index: u64) -> Option<usize> {
        match self.slots.get(&output_index) {
            Some(Slot::Text(i)) => Some(*i),
            _ => None,
        }
    }

    fn tool_index(&self, output_index: u64) -> Option<usize> {
        match self.slots.get(&output_index) {
            Some(Slot::ToolCall(i)) => Some(*i),
            _ => None,
        }
    }

    fn push_thinking(&mut self, output_index: u64, delta: &str, out: &mut Vec<StreamEvent>) {
        if let Some(i) = self.thinking_index(output_index)
            && let Some(Partial::Thinking { text, .. }) = self.content.get_mut(i)
        {
            text.push_str(delta);
            out.push(StreamEvent::ThinkingDelta(delta.to_owned()));
        }
    }

    fn push_text(&mut self, output_index: u64, delta: &str, out: &mut Vec<StreamEvent>) {
        if let Some(i) = self.text_index(output_index)
            && let Some(Partial::Text { text, .. }) = self.content.get_mut(i)
        {
            text.push_str(delta);
            out.push(StreamEvent::TextDelta(delta.to_owned()));
        }
    }

    fn push_arguments(&mut self, output_index: u64, delta: &str, out: &mut Vec<StreamEvent>) {
        if let Some(i) = self.tool_index(output_index)
            && let Some(Partial::ToolCall {
                partial_json: Some(json),
                ..
            }) = self.content.get_mut(i)
        {
            json.push_str(delta);
            out.push(StreamEvent::ToolCallArgsDelta(delta.to_owned()));
        }
    }

    fn arguments_done(&mut self, output_index: u64, arguments: &str, out: &mut Vec<StreamEvent>) {
        if let Some(i) = self.tool_index(output_index)
            && let Some(Partial::ToolCall {
                partial_json: Some(json),
                ..
            }) = self.content.get_mut(i)
        {
            if let Some(rest) = arguments.strip_prefix(json.as_str())
                && !rest.is_empty()
            {
                out.push(StreamEvent::ToolCallArgsDelta(rest.to_owned()));
            }
            arguments.clone_into(json);
        }
    }

    fn item_done(&mut self, output_index: u64, item: &Value, out: &mut Vec<StreamEvent>) {
        if !self.slots.contains_key(&output_index) {
            self.create_slot(output_index, item, out);
        }
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match (kind, self.slots.get(&output_index)) {
            ("reasoning", Some(Slot::Thinking(i))) => {
                let i = *i;
                let joined = |field: &str| {
                    item.get(field)
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default()
                };
                let summary = joined("summary");
                let content_text = joined("content");
                if let Some(Partial::Thinking {
                    text,
                    signature,
                    item_id,
                }) = self.content.get_mut(i)
                {
                    if !summary.is_empty() {
                        *text = summary;
                    } else if !content_text.is_empty() {
                        *text = content_text;
                    }
                    *signature = Some(item.to_string());
                    *item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
                    out.push(StreamEvent::ThinkingEnd(AssistantBlock::Thinking {
                        text: text.clone(),
                        signature: signature.clone(),
                        redacted: false,
                    }));
                }
                self.slots.remove(&output_index);
            }
            ("message", Some(Slot::Text(i))) => {
                let i = *i;
                let final_text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                                Some("output_text") => p.get("text").and_then(Value::as_str),
                                _ => p.get("refusal").and_then(Value::as_str),
                            })
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                let id = str_field(item, "id");
                let phase = item.get("phase").and_then(Value::as_str);
                if phase == Some("final_answer") {
                    self.stop = Some(StopReason::EndTurn);
                }
                if let Some(Partial::Text { text, signature }) = self.content.get_mut(i) {
                    *text = final_text;
                    *signature = Some(text_signature(&id, phase));
                }
                out.push(StreamEvent::TextEnd);
                self.slots.remove(&output_index);
            }
            ("function_call", Some(Slot::ToolCall(i))) => {
                let i = *i;
                if let Some(Partial::ToolCall {
                    id,
                    name,
                    partial_json,
                    args,
                }) = self.content.get_mut(i)
                    && let Some(streamed) = partial_json.take()
                {
                    let raw = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|a| !a.is_empty())
                        .map_or(streamed, str::to_owned);
                    *args = parse_tool_arguments(&raw);
                    out.push(StreamEvent::ToolCallEnd(AssistantBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                        provider_signature: None,
                    }));
                }
                self.slots.remove(&output_index);
            }
            _ => {}
        }
    }

    fn finalize(&mut self, response: &Value) -> Result<(), ClassifiedError> {
        self.terminal = true;
        // Backfill encrypted reasoning that `output_item.done` left out.
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            for item in output {
                if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                    continue;
                }
                let Some(encrypted) = item.get("encrypted_content").filter(|e| !e.is_null()) else {
                    continue;
                };
                let Some(item_id_value) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                for partial in &mut self.content {
                    if let Partial::Thinking {
                        signature: Some(sig),
                        item_id: Some(id),
                        ..
                    } = partial
                        && id == item_id_value
                        && let Ok(Value::Object(mut stored)) = serde_json::from_str::<Value>(sig)
                        && stored
                            .get("encrypted_content")
                            .is_none_or(|e| e.is_null() || e.as_str() == Some(""))
                    {
                        stored.insert("encrypted_content".into(), encrypted.clone());
                        *sig = Value::Object(stored).to_string();
                    }
                }
            }
        }
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
            let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
            let input = n(usage.get("input_tokens"));
            let cached = n(usage.pointer("/input_tokens_details/cached_tokens"));
            let cache_write = n(usage.pointer("/input_tokens_details/cache_write_tokens"));
            self.usage = CallUsage {
                input_tokens: input.saturating_sub(cached).saturating_sub(cache_write),
                output_tokens: n(usage.get("output_tokens")),
                thinking_tokens: n(usage.pointer("/output_tokens_details/reasoning_tokens")),
                cache_read_tokens: cached,
                cache_write_tokens: cache_write,
            };
        }
        let status = response.get("status").and_then(Value::as_str);
        let incomplete_reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        let stop = match status {
            None | Some("completed" | "in_progress" | "queued") => StopReason::EndTurn,
            Some("incomplete") if incomplete_reason == Some("max_output_tokens") => {
                StopReason::MaxTokens
            }
            Some("incomplete") => {
                let reason = incomplete_reason.unwrap_or("no reason given");
                // A content filter won't pass on a second try.
                return Err(ClassifiedError::new(
                    ProviderError {
                        kind: ProviderErrorKind::Other,
                        status: None,
                        message: format!("the response was incomplete: {reason}"),
                    },
                    ErrorClass::Fatal,
                ));
            }
            Some(other) => {
                return Err(ClassifiedError::new(
                    ProviderError {
                        kind: ProviderErrorKind::Server,
                        status: None,
                        message: format!("the response ended with status {other}"),
                    },
                    ErrorClass::Retryable,
                ));
            }
        };
        let has_tool_calls = self
            .content
            .iter()
            .any(|p| matches!(p, Partial::ToolCall { .. }));
        self.stop = Some(if stop == StopReason::EndTurn && has_tool_calls {
            StopReason::ToolUse
        } else {
            stop
        });
        Ok(())
    }

    fn message(&self) -> Message {
        let content = self
            .content
            .iter()
            .map(|p| match p {
                Partial::Thinking {
                    text, signature, ..
                } => AssistantBlock::Thinking {
                    text: text.clone(),
                    signature: signature.clone(),
                    redacted: false,
                },
                Partial::Text { text, signature } => AssistantBlock::Text {
                    text: text.clone(),
                    signature: signature.clone(),
                },
                Partial::ToolCall { id, name, args, .. } => AssistantBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    provider_signature: None,
                },
            })
            .collect();
        Message::Assistant {
            content,
            provider: self.provider,
            api: self.api,
            model: self.model.clone(),
            stop: self.stop.unwrap_or(StopReason::Unknown),
            aborted: false,
            failed: false,
            usage: self.usage,
            response_id: self.response_id.clone(),
        }
    }
}

impl StreamParser for ResponsesParser {
    fn feed(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, ClassifiedError> {
        let mut out = Vec::new();
        if self.terminal || event.data.is_empty() || event.data.trim() == "[DONE]" {
            return Ok(out);
        }
        let value = parse_event_json(&event.data).map_err(|e| {
            ClassifiedError::new(
                ProviderError {
                    kind: ProviderErrorKind::Other,
                    status: None,
                    message: format!("the stream sent an event that isn't JSON: {e}"),
                },
                ErrorClass::Retryable,
            )
        })?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or_default()
            .to_owned();
        let index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let delta = value
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind.as_str() {
            "response.created" => {
                if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item") {
                    self.create_slot(index, item, &mut out);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.push_thinking(index, delta, &mut out);
            }
            "response.reasoning_summary_part.done" => {
                self.push_thinking(index, "\n\n", &mut out);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.push_text(index, delta, &mut out);
            }
            "response.function_call_arguments.delta" => {
                self.push_arguments(index, delta, &mut out);
            }
            "response.function_call_arguments.done" => {
                let arguments = value
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.arguments_done(index, arguments, &mut out);
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    self.item_done(index, item, &mut out);
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                let response = value.get("response").cloned().unwrap_or(Value::Null);
                self.finalize(&response)?;
            }
            "error" | "response.failed" => {
                let vendor = if self.provider == Provider::Xai {
                    Flavour::Xai.vendor()
                } else {
                    Flavour::Platform.vendor()
                };
                return Err(stream_error(vendor, &value));
            }
            other => {
                tracing::debug!(event = other, "responses: ignoring a stream event");
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
            return Err(ClassifiedError::new(
                ProviderError {
                    kind: ProviderErrorKind::Network,
                    status: None,
                    message: "the response stream ended before its terminal event".into(),
                },
                ErrorClass::Retryable,
            ));
        }
        if self.stop == Some(StopReason::ToolUse) {
            for partial in &self.content {
                if let Partial::ToolCall {
                    name,
                    partial_json: Some(_),
                    ..
                } = partial
                {
                    return Err(ClassifiedError::new(
                        ProviderError {
                            kind: ProviderErrorKind::Other,
                            status: None,
                            message: format!(
                                "the response completed with an unfinished tool call ({name})"
                            ),
                        },
                        ErrorClass::Retryable,
                    ));
                }
            }
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

fn str_field(item: &Value, field: &str) -> String {
    item.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// An `error` or `response.failed` event as a classified error.
fn stream_error(vendor: &str, value: &Value) -> ClassifiedError {
    let failed = value.get("type").and_then(Value::as_str) == Some("response.failed");
    let (code, message) = if failed {
        (
            value
                .pointer("/response/error/code")
                .and_then(Value::as_str),
            value
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .or_else(|| {
                    value
                        .pointer("/response/incomplete_details/reason")
                        .and_then(Value::as_str)
                }),
        )
    } else {
        (
            value
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/error/code").and_then(Value::as_str)),
            value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/error/message").and_then(Value::as_str)),
        )
    };
    classify_code(vendor, None, code, message, None)
}

/// Tool arguments: an empty string is `{}`; valid JSON is kept; anything else stays the raw
/// string, which the loop answers with an error result.
pub fn parse_tool_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    match parse_event_json(raw) {
        Ok(v) if v.is_object() => v,
        _ => Value::String(raw.to_owned()),
    }
}

/// Parses JSON, tolerating lone UTF-16 surrogate escapes (`\ud83d` without its pair), which
/// `serde_json` refuses: they become U+FFFD (Pi's `unicode-surrogate` handling).
pub(crate) fn parse_event_json(text: &str) -> Result<Value, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(first) => {
            let repaired = replace_lone_surrogates(text);
            if repaired == text {
                Err(first)
            } else {
                serde_json::from_str(&repaired)
            }
        }
    }
}

fn replace_lone_surrogates(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let hex_at = |at: usize| -> Option<u16> {
        let s = text.get(at..at + 4)?;
        u16::from_str_radix(s, 16).ok()
    };
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'u'
                && let Some(unit) = hex_at(i + 2)
            {
                let high = (0xD800..=0xDBFF).contains(&unit);
                let low = (0xDC00..=0xDFFF).contains(&unit);
                if high {
                    let paired = text.get(i + 6..i + 8) == Some("\\u")
                        && hex_at(i + 8).is_some_and(|u| (0xDC00..=0xDFFF).contains(&u));
                    if paired {
                        out.push_str(&text[i..i + 12]);
                        i += 12;
                        continue;
                    }
                    out.push_str("\\ufffd");
                    i += 6;
                    continue;
                }
                if low {
                    out.push_str("\\ufffd");
                    i += 6;
                    continue;
                }
            }
            // Any other escape: copy both characters so `\\u` isn't misread.
            out.push_str(&text[i..i + 2]);
            i += 2;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// =============================================================================================
// Errors
// =============================================================================================

/// Classifies a failed HTTP response; `vendor` names the provider in the message (`OpenAI`,
/// `xAI`).
pub fn classify_http_error(
    vendor: &str,
    status: u16,
    headers: &HeaderMap,
    body: &str,
    now: Timestamp,
) -> ClassifiedError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let code = error
        .and_then(|e| e.get("code").or_else(|| e.get("type")))
        .and_then(Value::as_str)
        .or_else(|| parsed.as_ref()?.get("code")?.as_str());
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| parsed.as_ref()?.get("message")?.as_str());
    let resets_at = error
        .and_then(|e| e.get("resets_at"))
        .and_then(Value::as_i64)
        .and_then(|s| Timestamp::from_second(s).ok())
        .or_else(|| {
            error
                .and_then(|e| e.get("resets_in_seconds"))
                .and_then(Value::as_f64)
                .and_then(|s| now.checked_add(SignedDuration::from_secs_f64(s)).ok())
        })
        .or_else(|| {
            codex_usage_windows(headers, now)
                .into_iter()
                .filter(|w| w.used_percent >= 100.0)
                .find_map(|w| w.resets_at)
        });
    let mut classified = classify_code(vendor, Some(status), code, message, resets_at);
    if !matches!(
        classified.class,
        ErrorClass::AccountLimit { .. } | ErrorClass::Unauthorized | ErrorClass::ContextOverflow
    ) {
        match header_str(headers, "x-should-retry") {
            Some("true") => classified.class = ErrorClass::Retryable,
            Some("false") => classified.class = ErrorClass::Fatal,
            _ => {}
        }
    }
    classified
}

/// Classifies by status (if any), error code and message.
fn classify_code(
    vendor: &str,
    status: Option<u16>,
    code: Option<&str>,
    message: Option<&str>,
    resets_at: Option<Timestamp>,
) -> ClassifiedError {
    let code_lc = code.unwrap_or_default().to_ascii_lowercase();
    let message_text = message.unwrap_or_default();
    let message_lc = message_text.to_ascii_lowercase();
    let detail = match (code, message) {
        (Some(c), Some(m)) => format!("{c}: {}", cut(m, 300)),
        (Some(c), None) => c.to_owned(),
        (None, Some(m)) => cut(m, 300),
        (None, None) => "no details".to_owned(),
    };
    let label = status.map_or_else(
        || format!("the {vendor} stream reported an error"),
        |s| format!("{vendor} API error (HTTP {s})"),
    );
    let error = |kind: ProviderErrorKind| ProviderError {
        kind,
        status,
        message: format!("{label}: {detail}"),
    };

    let usage_limit = USAGE_LIMIT_CODES.iter().any(|c| code_lc == *c)
        || USAGE_LIMIT_CODES.iter().any(|c| message_lc.contains(c))
        || message_lc.contains("usage limit");
    if usage_limit && status.is_none_or(|s| s == 429 || s == 403 || s == 402) {
        let mut e = error(ProviderErrorKind::RateLimited);
        if code_lc == "subscription_sharing_usage_limit_exceeded"
            || message_lc.contains("subscription_sharing_usage_limit_exceeded")
        {
            e.message
                .push_str(" (check your ChatGPT usage: https://chatgpt.com/settings/usage)");
        }
        return ClassifiedError::new(
            e,
            ErrorClass::AccountLimit {
                window: None,
                resets_at,
            },
        );
    }
    if status == Some(401) {
        return ClassifiedError::new(error(ProviderErrorKind::Auth), ErrorClass::Unauthorized);
    }
    let overflow = code_lc == "context_length_exceeded"
        || message_lc.contains("context window")
        || message_lc.contains("context length")
        || message_lc.contains("maximum context")
        || message_lc.contains("too many tokens")
        || message_lc.contains("input exceeds");
    if overflow && status.is_none_or(|s| s == 400 || s == 413) {
        return ClassifiedError::new(
            error(ProviderErrorKind::ContextOverflow),
            ErrorClass::ContextOverflow,
        );
    }
    match status {
        Some(408) => ClassifiedError::new(error(ProviderErrorKind::Network), ErrorClass::Retryable),
        Some(409) => ClassifiedError::new(error(ProviderErrorKind::Other), ErrorClass::Retryable),
        Some(429) => {
            ClassifiedError::new(error(ProviderErrorKind::RateLimited), ErrorClass::Retryable)
        }
        Some(503 | 529) => {
            ClassifiedError::new(error(ProviderErrorKind::Overloaded), ErrorClass::Retryable)
        }
        Some(s) if s >= 500 => {
            ClassifiedError::new(error(ProviderErrorKind::Server), ErrorClass::Retryable)
        }
        Some(403) => ClassifiedError::new(error(ProviderErrorKind::Auth), ErrorClass::Fatal),
        Some(_) => {
            ClassifiedError::new(error(ProviderErrorKind::InvalidRequest), ErrorClass::Fatal)
        }
        None => {
            // An in-stream error: transient-sounding ones are worth a retry.
            let transient = code_lc.contains("server_error")
                || code_lc.contains("rate_limit")
                || message_lc.contains("overloaded")
                || message_lc.contains("try again")
                || message_lc.contains("rate limit")
                || message_lc.contains("service unavailable");
            if transient {
                ClassifiedError::new(error(ProviderErrorKind::Server), ErrorClass::Retryable)
            } else {
                ClassifiedError::new(error(ProviderErrorKind::Other), ErrorClass::Fatal)
            }
        }
    }
}

pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut s: String = text.chars().take(max).collect();
        s.push('…');
        s
    }
}

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// The ChatGPT backend's usage headers (`x-codex-primary-used-percent`,
/// `x-codex-primary-reset-after-seconds`, `x-codex-primary-window-minutes`, and the
/// `secondary` set) as usage windows. A 300-minute window is `five_hour`, a 10,080-minute one
/// `seven_day`; others are named `primary`/`secondary`.
pub fn codex_usage_windows(headers: &HeaderMap, now: Timestamp) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    for slot in ["primary", "secondary"] {
        let Some(used) = header_str(headers, &format!("x-codex-{slot}-used-percent"))
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        else {
            continue;
        };
        let resets_at = header_str(headers, &format!("x-codex-{slot}-reset-after-seconds"))
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .and_then(|s| now.checked_add(SignedDuration::from_secs_f64(s)).ok());
        let minutes = header_str(headers, &format!("x-codex-{slot}-window-minutes"))
            .and_then(|v| v.parse::<u64>().ok());
        let name = match minutes {
            Some(300) => "five_hour".to_owned(),
            Some(10_080) => "seven_day".to_owned(),
            _ => slot.to_owned(),
        };
        windows.push(UsageWindow {
            name,
            used_percent: used,
            resets_at,
        });
    }
    windows
}
