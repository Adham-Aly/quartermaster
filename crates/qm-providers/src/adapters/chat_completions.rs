//! OpenAI Chat Completions with per-provider compatibility flags.
//!
//! A Rust port of Pi's `packages/ai/src/api/openai-completions.ts` (and
//! `github-copilot-headers.ts`, `openai-prompt-cache.ts`) at the pinned commit `1402973a` (MIT,
//! Copyright Earendil Works). It serves OpenRouter, Kimi, GitHub Copilot and custom
//! OpenAI-compatible endpoints. Grok (xAI) moved to the Responses adapter, whose `reasoning.effort`
//! xAI honours.
//!
//! - **Request**: `POST <base>/chat/completions` with `model`, `messages` (the system prompt
//!   first, as `developer` for reasoning models where the provider accepts it, else `system`),
//!   `stream: true`, `stream_options.include_usage`, `store: false`, the output cap as
//!   `max_completion_tokens` (or `max_tokens`), `tools` (`[]` when the history has tool calls but
//!   none are offered) and the reasoning setting.
//! - **Compatibility flags** come from the catalog's corrections ([`Compat`]); an
//!   absent flag means OpenAI's own behaviour, which is what a custom endpoint gets.
//! - **Effort** (through the catalog): `reasoning_effort: <value>`, or OpenRouter's
//!   `reasoning: {effort}`; a rung mapped to `off` sends OpenRouter's `none` and nothing
//!   elsewhere; an unsupported rung, or a provider without reasoning effort, sends nothing.
//! - **Replay**: thinking is kept with the name of the field it streamed in (`reasoning_content`,
//!   `reasoning`, `reasoning_text`) as its signature, or OpenRouter's `reasoning_details` (JSON)
//!   when they streamed, and goes back that way to the same model. Tool-call IDs from another
//!   model: a Responses `call_id|item_id` becomes `call_id_item_id` (sanitised, at most 40
//!   characters, else shortened with a hash); others pass unchanged. Tool-result images go in a
//!   user message after the results, for models that take images.
//! - **Stream**: `chat.completion.chunk`s until `[DONE]` (or the end of the body). Blocks end
//!   when the stream does, in order. A stream without a `finish_reason` is an error;
//!   `content_filter` is an error that isn't retried; an `error` object in a chunk is reported.
//!
//! Not ported: grammar ("custom") tools, mid-conversation system messages and tool additions,
//! the other providers' thinking formats (z.ai, Qwen, `DeepSeek`, Together, chat templates),
//! thinking-token budgets, routing preferences.

use super::common::{self, EffortSetting};
use super::responses::{data_url, header_str, parse_event_json, parse_tool_arguments, short_hash};
use super::{
    AdapterError, ClassifiedError, ErrorClass, HeaderMap, HttpRequest, ModelRequest, StreamEvent,
    StreamParser, WireAdapter, WireApi,
};
use crate::sse::SseEvent;
use crate::transcript::{AssistantBlock, Block, Message};
use qm_engine::{CallUsage, ProviderErrorKind, StopReason};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// OpenRouter's base URL.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Kimi For Coding's base URL (the Kimi Code subscription and its API keys).
pub const KIMI_BASE_URL: &str = "https://api.kimi.com/coding/v1";
/// GitHub Copilot's base URL for individual plans (business and enterprise accounts set theirs).
pub const COPILOT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
/// The longest prompt-cache key OpenAI accepts.
const PROMPT_CACHE_KEY_MAX: usize = 64;
/// Chat Completions tool-call IDs may be at most this long (OpenAI's limit).
const TOOL_CALL_ID_MAX: usize = 40;
/// The headers Copilot's API expects from a chat client (Pi's catalog sends the same).
const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

// ---- Compatibility flags, as `catalog/corrections.toml` spells them. ----

/// One Chat Completions compatibility flag. The catalog's corrections set them per provider and per
/// model; an absent flag takes [`CompatFlag::default_value`], OpenAI's own behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompatFlag {
    /// Send `store: false` (default on).
    SupportsStore,
    /// The system prompt may be a `developer` message for reasoning models (default on).
    SupportsDeveloperRole,
    /// The reasoning setting may be sent (default on).
    SupportsReasoningEffort,
    /// `stream_options: {include_usage: true}` (default on).
    SupportsUsageInStreaming,
    /// A stream ends with a `finish_reason` (default on); without it, a stream is complete when
    /// it ends.
    SupportsFinishReason,
    /// Tools carry `strict: false` (default off: some providers reject the field).
    SupportsStrictMode,
    /// The output cap is `max_tokens`, not `max_completion_tokens` (default off).
    UsesMaxTokens,
    /// Reasoning as OpenRouter's `reasoning: {effort}` (default off).
    OpenRouterReasoning,
    /// Assistant messages of reasoning models need `reasoning_content` (default off; Kimi).
    RequiresReasoningContent,
    /// Send the session ID as `x-session-id` (default off; OpenRouter's session affinity).
    SessionAffinity,
    /// Anthropic-style `cache_control` on the system prompt, the last tool and the last message
    /// (default off; Claude through OpenRouter).
    AnthropicCacheControl,
    /// Copilot's client headers (default off).
    CopilotHeaders,
    /// Tool results carry the tool's `name` (default off).
    RequiresToolResultName,
    /// A user message may not follow tool results directly (default off).
    RequiresAssistantAfterToolResult,
    /// Replayed thinking goes back as text (default off).
    RequiresThinkingAsText,
}

impl CompatFlag {
    /// Every flag.
    pub const ALL: [Self; 15] = [
        Self::SupportsStore,
        Self::SupportsDeveloperRole,
        Self::SupportsReasoningEffort,
        Self::SupportsUsageInStreaming,
        Self::SupportsFinishReason,
        Self::SupportsStrictMode,
        Self::UsesMaxTokens,
        Self::OpenRouterReasoning,
        Self::RequiresReasoningContent,
        Self::SessionAffinity,
        Self::AnthropicCacheControl,
        Self::CopilotHeaders,
        Self::RequiresToolResultName,
        Self::RequiresAssistantAfterToolResult,
        Self::RequiresThinkingAsText,
    ];

    /// The name in the corrections file.
    pub const fn name(self) -> &'static str {
        match self {
            Self::SupportsStore => "supports_store",
            Self::SupportsDeveloperRole => "supports_developer_role",
            Self::SupportsReasoningEffort => "supports_reasoning_effort",
            Self::SupportsUsageInStreaming => "supports_usage_in_streaming",
            Self::SupportsFinishReason => "supports_finish_reason",
            Self::SupportsStrictMode => "supports_strict_mode",
            Self::UsesMaxTokens => "uses_max_tokens",
            Self::OpenRouterReasoning => "openrouter_reasoning",
            Self::RequiresReasoningContent => "requires_reasoning_content",
            Self::SessionAffinity => "session_affinity",
            Self::AnthropicCacheControl => "anthropic_cache_control",
            Self::CopilotHeaders => "copilot_headers",
            Self::RequiresToolResultName => "requires_tool_result_name",
            Self::RequiresAssistantAfterToolResult => "requires_assistant_after_tool_result",
            Self::RequiresThinkingAsText => "requires_thinking_as_text",
        }
    }

    /// The value when the catalog doesn't set it: OpenAI's own behaviour.
    pub const fn default_value(self) -> bool {
        matches!(
            self,
            Self::SupportsStore
                | Self::SupportsDeveloperRole
                | Self::SupportsReasoningEffort
                | Self::SupportsUsageInStreaming
                | Self::SupportsFinishReason
        )
    }

    const fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// One model's compatibility flags, resolved from the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compat {
    on: u32,
}

impl Default for Compat {
    /// OpenAI's own behaviour.
    fn default() -> Self {
        Self::from_fn(CompatFlag::default_value)
    }
}

impl Compat {
    fn from_fn(value: impl Fn(CompatFlag) -> bool) -> Self {
        Self {
            on: CompatFlag::ALL
                .iter()
                .filter(|f| value(**f))
                .fold(0, |on, f| on | f.bit()),
        }
    }

    /// The flags the catalog holds for a provider's model, over [`Compat::default`].
    pub fn resolve(provider: Provider, model: &ModelId) -> Self {
        let flags = common::compat_flags(provider, model);
        Self::from_fn(|f| flags.get(f.name()).copied().unwrap_or(f.default_value()))
    }

    /// Whether `flag` is on.
    pub const fn has(self, flag: CompatFlag) -> bool {
        self.on & flag.bit() != 0
    }
}

/// The Chat Completions adapter for one provider.
#[derive(Clone, Copy, Debug)]
pub struct ChatCompletionsAdapter {
    provider: Provider,
}

impl ChatCompletionsAdapter {
    pub const fn new(provider: Provider) -> Self {
        Self { provider }
    }

    /// The provider's own base URL; a custom endpoint has none (its account sets one).
    pub const fn default_base_url(provider: Provider) -> Option<&'static str> {
        match provider {
            Provider::OpenRouter => Some(OPENROUTER_BASE_URL),
            Provider::Kimi => Some(KIMI_BASE_URL),
            Provider::Copilot => Some(COPILOT_BASE_URL),
            _ => None,
        }
    }

    fn base_url(self, base_url: Option<&str>) -> Result<String, AdapterError> {
        base_url
            .filter(|b| !b.trim().is_empty())
            .or_else(|| Self::default_base_url(self.provider))
            .map(|b| b.trim_end_matches('/').to_owned())
            .ok_or_else(|| {
                AdapterError::Invalid(format!(
                    "a {} account needs a base URL for its OpenAI-compatible endpoint",
                    self.provider
                ))
            })
    }
}

impl WireAdapter for ChatCompletionsAdapter {
    fn reported_usage(&self) -> qm_engine::ReportedUsage {
        qm_engine::ReportedUsage::NONE
            .with_thinking()
            .with_cache_read()
    }

    fn api(&self) -> WireApi {
        WireApi::OpenAiCompletions
    }

    fn build_request(&self, req: &ModelRequest<'_>) -> Result<HttpRequest, AdapterError> {
        let base = self.base_url(req.base_url)?;
        let compat = Compat::resolve(req.provider, &req.model.id);

        let mut messages = convert_messages(req, compat);
        let mut tools = if req.tools.is_empty() {
            // Some providers (Claude behind a proxy) require `tools` once the history has tool
            // calls or results.
            has_tool_history(req.messages).then(Vec::new)
        } else {
            Some(convert_tools(req, compat))
        };
        if compat.has(CompatFlag::AnthropicCacheControl) {
            let cache_control = json!({ "type": "ephemeral" });
            add_cache_control(&mut messages, tools.as_deref_mut(), &cache_control);
        }

        let mut body = Map::new();
        body.insert("model".into(), json!(req.model.id.as_str()));
        body.insert("messages".into(), Value::Array(messages));
        body.insert("stream".into(), json!(true));
        if base.contains("api.openai.com") {
            body.insert(
                "prompt_cache_key".into(),
                json!(
                    req.session_id
                        .chars()
                        .take(PROMPT_CACHE_KEY_MAX)
                        .collect::<String>()
                ),
            );
        }
        if compat.has(CompatFlag::SupportsUsageInStreaming) {
            body.insert("stream_options".into(), json!({ "include_usage": true }));
        }
        if compat.has(CompatFlag::SupportsStore) {
            body.insert("store".into(), json!(false));
        }
        if let Some(cap) = req
            .max_output_tokens
            .filter(|_| self.can_send_output_cap(req))
        {
            let field = if compat.has(CompatFlag::UsesMaxTokens) {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            body.insert(field.into(), json!(cap));
        }
        if let Some(tools) = tools {
            body.insert("tools".into(), Value::Array(tools));
        }
        if compat.has(CompatFlag::SupportsReasoningEffort) {
            match (
                common::effort_setting(req),
                compat.has(CompatFlag::OpenRouterReasoning),
            ) {
                (EffortSetting::Value(effort), true) => {
                    body.insert("reasoning".into(), json!({ "effort": effort }));
                }
                (EffortSetting::Off, true) => {
                    body.insert("reasoning".into(), json!({ "effort": "none" }));
                }
                (EffortSetting::Value(effort), false) => {
                    body.insert("reasoning_effort".into(), json!(effort));
                }
                (EffortSetting::Off | EffortSetting::Omit, false) | (EffortSetting::Omit, true) => {
                }
            }
        }

        let mut headers = vec![
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        if compat.has(CompatFlag::CopilotHeaders) {
            headers.extend(
                COPILOT_HEADERS
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
            );
            headers.extend(copilot_dynamic_headers(req.messages));
        } else {
            headers.push(("User-Agent".to_owned(), req.user_agent.to_owned()));
        }
        if compat.has(CompatFlag::SessionAffinity) {
            headers.push(("x-session-id".to_owned(), req.session_id.to_owned()));
        }

        let url = if base.ends_with("/chat/completions") {
            base
        } else {
            format!("{base}/chat/completions")
        };
        Ok(HttpRequest {
            url,
            headers,
            body: Value::Object(body),
        })
    }

    fn parser(&self, req: &ModelRequest<'_>) -> Box<dyn StreamParser> {
        let compat = Compat::resolve(req.provider, &req.model.id);
        Box::new(ChatCompletionsParser::new(
            req.provider,
            req.model.id.clone(),
            compat.has(CompatFlag::SupportsFinishReason),
        ))
    }

    fn classify_error(&self, status: u16, headers: &HeaderMap, body: &str) -> ClassifiedError {
        classify_http_error(self.provider, status, headers, body)
    }

    fn normalize_tool_call_id(&self, id: &str, _from_foreign_model: bool) -> String {
        normalize_tool_call_id(id)
    }

    fn can_send_output_cap(&self, _req: &ModelRequest<'_>) -> bool {
        true
    }
}

/// Copilot's per-request headers: who started the request (the user, or the agent following up
/// on its own tool calls), the intent, and whether it carries images.
fn copilot_dynamic_headers(messages: &[Message]) -> Vec<(String, String)> {
    let initiator = match messages.last() {
        Some(Message::User { .. }) | None => "user",
        Some(_) => "agent",
    };
    let mut headers = vec![
        ("X-Initiator".to_owned(), initiator.to_owned()),
        ("Openai-Intent".to_owned(), "conversation-edits".to_owned()),
    ];
    let has_images = messages.iter().any(|m| match m {
        Message::User { content } | Message::ToolResult { content, .. } => {
            content.iter().any(|b| matches!(b, Block::Image { .. }))
        }
        Message::Assistant { .. } => false,
    });
    if has_images {
        headers.push(("Copilot-Vision-Request".to_owned(), "true".to_owned()));
    }
    headers
}

/// Pi's rule for Chat Completions: a Responses `call_id|item_id` keeps both parts (calls in one
/// turn may share a `call_id`), sanitised to `[A-Za-z0-9_-]` and joined with `_`; over 40
/// characters it becomes the call ID's start and an 8-character hash. Other IDs pass unchanged.
pub fn normalize_tool_call_id(id: &str) -> String {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return id.to_owned();
    };
    let sanitize = |part: &str| -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let call_id = sanitize(call_id);
    let item_id = sanitize(item_id);
    let combined = if item_id.is_empty() {
        call_id.clone()
    } else {
        format!("{call_id}_{item_id}")
    };
    if combined.chars().count() <= TOOL_CALL_ID_MAX {
        return combined;
    }
    let hash: String = short_hash(id).chars().take(8).collect();
    let keep = TOOL_CALL_ID_MAX.saturating_sub(hash.len() + 1).max(1);
    let prefix: String = call_id.chars().take(keep).collect();
    format!("{prefix}_{hash}")
}

// =============================================================================================
// Request conversion
// =============================================================================================

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::ToolResult { .. } => true,
        Message::Assistant { .. } => !m.tool_calls().is_empty(),
        Message::User { .. } => false,
    })
}

/// The `tools` array.
fn convert_tools(req: &ModelRequest<'_>, compat: Compat) -> Vec<Value> {
    req.tools
        .iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("name".into(), json!(tool.name));
            function.insert("description".into(), json!(tool.description));
            function.insert("parameters".into(), tool.input_schema.clone());
            if compat.has(CompatFlag::SupportsStrictMode) {
                function.insert("strict".into(), json!(false));
            }
            json!({ "type": "function", "function": function })
        })
        .collect()
}

fn image_part(media_type: &str, bytes: &[u8]) -> Value {
    json!({ "type": "image_url", "image_url": { "url": data_url(media_type, bytes) } })
}

/// The reasoning field a thinking block's signature names, if it names one.
fn reasoning_field(signature: &str) -> Option<&'static str> {
    match signature {
        "reasoning_content" => Some("reasoning_content"),
        "reasoning" => Some("reasoning"),
        "reasoning_text" => Some("reasoning_text"),
        _ => None,
    }
}

/// OpenRouter `reasoning_details` kept as a thinking block's signature, if valid.
fn parse_reasoning_details(signature: &str) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(signature).ok()?;
    let details = parsed.as_array()?;
    (!details.is_empty() && details.iter().all(is_reasoning_detail)).then(|| details.clone())
}

/// One assistant message (Pi's `convertMessages`, assistant branch), or `None` when it has
/// neither content nor tool calls.
fn convert_assistant(
    req: &ModelRequest<'_>,
    content: &[AssistantBlock],
    compat: Compat,
) -> Option<Value> {
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if compat.has(CompatFlag::RequiresAssistantAfterToolResult) {
            json!("")
        } else {
            Value::Null
        },
    );
    let text_parts: Vec<&str> = content
        .iter()
        .filter_map(|b| match b {
            AssistantBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text: String = text_parts.concat();
    let thinking: Vec<(&str, Option<&str>)> = content
        .iter()
        .filter_map(|b| match b {
            AssistantBlock::Thinking {
                text, signature, ..
            } => Some((text.as_str(), signature.as_deref())),
            _ => None,
        })
        .collect();
    let details = thinking
        .iter()
        .find_map(|(_, sig)| sig.and_then(parse_reasoning_details));
    let non_empty_thinking: Vec<(&str, Option<&str>)> = thinking
        .iter()
        .copied()
        .filter(|(t, _)| !t.trim().is_empty())
        .collect();
    if non_empty_thinking.is_empty() {
        if !text.is_empty() {
            msg.insert("content".into(), json!(text));
        }
    } else if compat.has(CompatFlag::RequiresThinkingAsText) {
        let thinking_text = non_empty_thinking
            .iter()
            .map(|(t, _)| *t)
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut parts = vec![json!({ "type": "text", "text": thinking_text })];
        parts.extend(
            text_parts
                .iter()
                .map(|t| json!({ "type": "text", "text": t })),
        );
        msg.insert("content".into(), Value::Array(parts));
    } else {
        if !text.is_empty() {
            msg.insert("content".into(), json!(text));
        }
        if details.is_none()
            && let Some(field) = non_empty_thinking[0].1.and_then(reasoning_field)
        {
            let joined = non_empty_thinking
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
                .join("\n");
            msg.insert(field.into(), json!(joined));
        }
    }
    let tool_calls: Vec<Value> = content
        .iter()
        .filter_map(|b| match b {
            AssistantBlock::ToolCall { id, name, args, .. } => Some(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": common::args_text(args) },
            })),
            _ => None,
        })
        .collect();
    let has_tool_calls = !tool_calls.is_empty();
    if has_tool_calls {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if let Some(details) = details {
        msg.insert("reasoning_details".into(), Value::Array(details));
    }
    if compat.has(CompatFlag::RequiresReasoningContent)
        && req.model.reasoning
        && !msg.contains_key("reasoning_content")
    {
        msg.insert("reasoning_content".into(), json!(""));
    }
    let has_content = match msg.get("content") {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    };
    (has_content || has_tool_calls).then_some(Value::Object(msg))
}

/// Our prepared transcript as Chat Completions messages (Pi's `convertMessages`).
fn convert_messages(req: &ModelRequest<'_>, compat: Compat) -> Vec<Value> {
    let mut params = Vec::new();
    if !req.system_prompt.is_empty() {
        let role = if req.model.reasoning && compat.has(CompatFlag::SupportsDeveloperRole) {
            "developer"
        } else {
            "system"
        };
        params.push(json!({ "role": role, "content": req.system_prompt }));
    }
    let bridge = || json!({ "role": "assistant", "content": "I have processed the tool results." });
    let mut last_was_tool_result = false;
    let mut i = 0;
    while i < req.messages.len() {
        match &req.messages[i] {
            Message::User { content } => {
                if compat.has(CompatFlag::RequiresAssistantAfterToolResult) && last_was_tool_result
                {
                    params.push(bridge());
                }
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } if text.is_empty() => None,
                        Block::Text { text } => Some(json!({ "type": "text", "text": text })),
                        Block::Image { media_type, bytes } => Some(image_part(media_type, bytes)),
                    })
                    .collect();
                if !parts.is_empty() {
                    params.push(json!({ "role": "user", "content": parts }));
                }
                last_was_tool_result = false;
            }
            Message::Assistant { content, .. } => {
                if let Some(msg) = convert_assistant(req, content, compat) {
                    params.push(msg);
                }
                last_was_tool_result = false;
            }
            Message::ToolResult { .. } => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult {
                    call_id,
                    name,
                    content,
                    ..
                }) = req.messages.get(i)
                {
                    let text = content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(text.as_str()),
                            Block::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = content.iter().any(|b| matches!(b, Block::Image { .. }));
                    let text = if !text.trim().is_empty() {
                        text
                    } else if has_images {
                        "(see attached image)".to_owned()
                    } else {
                        "(no tool output)".to_owned()
                    };
                    let mut msg = Map::new();
                    msg.insert("role".into(), json!("tool"));
                    msg.insert("content".into(), json!(text));
                    msg.insert("tool_call_id".into(), json!(call_id));
                    if compat.has(CompatFlag::RequiresToolResultName) && !name.is_empty() {
                        msg.insert("name".into(), json!(name));
                    }
                    params.push(Value::Object(msg));
                    if has_images && req.model.image_input {
                        images.extend(content.iter().filter_map(|b| match b {
                            Block::Image { media_type, bytes } => {
                                Some(image_part(media_type, bytes))
                            }
                            Block::Text { .. } => None,
                        }));
                    }
                    i += 1;
                }
                if images.is_empty() {
                    last_was_tool_result = true;
                } else {
                    if compat.has(CompatFlag::RequiresAssistantAfterToolResult) {
                        params.push(bridge());
                    }
                    let mut parts = vec![
                        json!({ "type": "text", "text": "Attached image(s) from tool result:" }),
                    ];
                    parts.extend(images);
                    params.push(json!({ "role": "user", "content": parts }));
                    last_was_tool_result = false;
                }
                continue;
            }
        }
        i += 1;
    }
    params
}

/// Anthropic-style caching through an OpenAI-compatible API: the system prompt, the last tool and
/// the last conversation message with text.
fn add_cache_control(messages: &mut [Value], tools: Option<&mut [Value]>, cache_control: &Value) {
    if let Some(system) = messages.iter_mut().find(|m| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        )
    }) {
        add_cache_control_to_text(system, cache_control);
    }
    if let Some(last) = tools
        .and_then(|t| t.last_mut())
        .and_then(Value::as_object_mut)
    {
        last.insert("cache_control".into(), cache_control.clone());
    }
    for message in messages.iter_mut().rev() {
        let role = message.get("role").and_then(Value::as_str);
        if matches!(role, Some("user" | "assistant" | "tool"))
            && add_cache_control_to_text(message, cache_control)
        {
            return;
        }
    }
}

/// Puts `cache_control` on a message's (last) text; `false` when it has none.
fn add_cache_control_to_text(message: &mut Value, cache_control: &Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    match content {
        Value::String(text) if !text.is_empty() => {
            *content =
                json!([{ "type": "text", "text": text.clone(), "cache_control": cache_control }]);
            true
        }
        Value::Array(parts) => {
            if let Some(part) = parts
                .iter_mut()
                .rev()
                .find(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(Value::as_object_mut)
            {
                part.insert("cache_control".into(), cache_control.clone());
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

// =============================================================================================
// Stream parsing
// =============================================================================================

/// A valid OpenRouter reasoning detail.
fn is_reasoning_detail(detail: &Value) -> bool {
    let Some(obj) = detail.as_object() else {
        return false;
    };
    let common_ok = obj.get("id").is_none_or(|v| v.is_null() || v.is_string())
        && obj.get("format").is_none_or(Value::is_string)
        && obj.get("index").is_none_or(Value::is_number);
    if !common_ok {
        return false;
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => obj.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => obj.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            obj.get("text").is_some_and(Value::is_string)
                && obj
                    .get("signature")
                    .is_none_or(|v| v.is_null() || v.is_string())
        }
        _ => false,
    }
}

/// Appends a streamed detail, merging consecutive text and summary deltas into one entry.
fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let kind = detail.get("type").and_then(Value::as_str);
    if let Some(last) = details.last_mut()
        && last.get("type").and_then(Value::as_str) == kind
        && let (Some(last), Some(detail)) = (last.as_object_mut(), detail.as_object())
    {
        let merged_field = match kind {
            Some("reasoning.text") => Some("text"),
            Some("reasoning.summary") => Some("summary"),
            _ => None,
        };
        if let Some(field) = merged_field {
            let addition = detail
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default();
            let joined = format!(
                "{}{addition}",
                last.get(field).and_then(Value::as_str).unwrap_or_default()
            );
            last.insert(field.into(), json!(joined));
            if field == "text"
                && last
                    .get("signature")
                    .is_none_or(|s| s.is_null() || s.as_str() == Some(""))
                && let Some(sig) = detail.get("signature").filter(|s| !s.is_null())
            {
                last.insert("signature".into(), sig.clone());
            }
            if last.get("id").is_none_or(Value::is_null)
                && let Some(id) = detail.get("id")
            {
                last.insert("id".into(), id.clone());
            }
            if last
                .get("format")
                .is_none_or(|f| f.as_str().is_none_or(str::is_empty))
                && let Some(format) = detail.get("format")
            {
                last.insert("format".into(), format.clone());
            }
            if last.get("index").is_none_or(Value::is_null)
                && let Some(index) = detail.get("index")
            {
                last.insert("index".into(), index.clone());
            }
            return;
        }
    }
    details.push(detail.clone());
}

#[derive(Debug)]
enum Partial {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        /// The field it streamed in (`reasoning_content`, …), or `""` for reasoning details.
        field: String,
    },
    ToolCall {
        id: String,
        name: String,
        json: String,
    },
}

/// Parses one Chat Completions stream (Pi's `stream`).
#[derive(Debug)]
pub struct ChatCompletionsParser {
    provider: Provider,
    model: ModelId,
    supports_finish_reason: bool,
    blocks: Vec<Partial>,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    tools_by_index: HashMap<u64, usize>,
    tools_by_id: HashMap<String, usize>,
    reasoning_details: Option<Vec<Value>>,
    response_id: Option<String>,
    usage: CallUsage,
    stop: Option<StopReason>,
    stop_error: Option<ClassifiedError>,
    terminal: bool,
    finished: bool,
}

impl ChatCompletionsParser {
    pub fn new(provider: Provider, model: ModelId, supports_finish_reason: bool) -> Self {
        Self {
            provider,
            model,
            supports_finish_reason,
            blocks: Vec::new(),
            text_block: None,
            thinking_block: None,
            tools_by_index: HashMap::new(),
            tools_by_id: HashMap::new(),
            reasoning_details: None,
            response_id: None,
            usage: CallUsage::default(),
            stop: None,
            stop_error: None,
            terminal: false,
            finished: false,
        }
    }

    fn thinking(&mut self, field: &str) -> usize {
        if let Some(i) = self.thinking_block {
            return i;
        }
        self.blocks.push(Partial::Thinking {
            text: String::new(),
            field: field.to_owned(),
        });
        let i = self.blocks.len() - 1;
        self.thinking_block = Some(i);
        i
    }

    fn text(&mut self) -> usize {
        if let Some(i) = self.text_block {
            return i;
        }
        self.blocks.push(Partial::Text {
            text: String::new(),
        });
        let i = self.blocks.len() - 1;
        self.text_block = Some(i);
        i
    }

    fn tool_call(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        let index = delta.get("index").and_then(Value::as_u64);
        let id = delta
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let name = delta
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let existing = index
            .and_then(|i| self.tools_by_index.get(&i).copied())
            .or_else(|| id.and_then(|id| self.tools_by_id.get(id).copied()));
        let at = if let Some(at) = existing {
            at
        } else {
            self.blocks.push(Partial::ToolCall {
                id: id.unwrap_or_default().to_owned(),
                name: name.to_owned(),
                json: String::new(),
            });
            out.push(StreamEvent::ToolCallStart {
                id: id.unwrap_or_default().to_owned(),
                name: name.to_owned(),
            });
            self.blocks.len() - 1
        };
        if let Some(index) = index {
            self.tools_by_index.entry(index).or_insert(at);
        }
        if let Some(id) = id {
            self.tools_by_id.insert(id.to_owned(), at);
        }
        let arguments = delta
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(Partial::ToolCall {
            id: block_id,
            name: block_name,
            json,
        }) = self.blocks.get_mut(at)
        {
            if block_id.is_empty()
                && let Some(id) = id
            {
                id.clone_into(block_id);
            }
            if block_name.is_empty() && !name.is_empty() {
                name.clone_into(block_name);
            }
            json.push_str(arguments);
        }
        // Pi reports a delta for every tool-call entry, even an empty one.
        out.push(StreamEvent::ToolCallArgsDelta(arguments.to_owned()));
    }

    fn delta(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        // Reasoning before content: some proxies put both in one delta, and thinking comes first.
        if let Some((field, piece)) = ["reasoning_content", "reasoning", "reasoning_text"]
            .iter()
            .find_map(|f| {
                delta
                    .get(*f)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| (*f, s))
            })
        {
            let i = self.thinking(field);
            if let Some(Partial::Thinking { text, .. }) = self.blocks.get_mut(i) {
                text.push_str(piece);
            }
            out.push(StreamEvent::ThinkingDelta(piece.to_owned()));
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details.iter().filter(|d| is_reasoning_detail(d)) {
                self.thinking("");
                append_reasoning_detail(
                    self.reasoning_details.get_or_insert_with(Vec::new),
                    detail,
                );
            }
        }
        if let Some(piece) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let i = self.text();
            if let Some(Partial::Text { text }) = self.blocks.get_mut(i) {
                text.push_str(piece);
            }
            out.push(StreamEvent::TextDelta(piece.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.tool_call(call, out);
            }
        }
    }

    fn thinking_signature(&self, field: &str) -> Option<String> {
        match &self.reasoning_details {
            Some(details) => Some(Value::Array(details.clone()).to_string()),
            None => Some(field.to_owned()).filter(|f| !f.is_empty()),
        }
    }

    fn content(&self) -> Vec<AssistantBlock> {
        self.blocks
            .iter()
            .map(|p| match p {
                Partial::Text { text } => AssistantBlock::Text {
                    text: text.clone(),
                    signature: None,
                },
                Partial::Thinking { text, field } => AssistantBlock::Thinking {
                    text: text.clone(),
                    signature: self.thinking_signature(field),
                    redacted: false,
                },
                Partial::ToolCall { id, name, json } => AssistantBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args: parse_tool_arguments(json),
                    provider_signature: None,
                },
            })
            .collect()
    }

    fn message(&self) -> Message {
        Message::Assistant {
            content: self.content(),
            provider: self.provider,
            api: WireApi::OpenAiCompletions,
            model: self.model.clone(),
            stop: self.stop.unwrap_or(StopReason::Unknown),
            aborted: false,
            failed: false,
            usage: self.usage,
            response_id: self.response_id.clone(),
        }
    }
}

/// Pi's `parseChunkUsage`.
fn parse_usage(usage: &Value) -> CallUsage {
    let n = |p: &str| usage.pointer(p).and_then(Value::as_u64);
    let prompt = n("/prompt_tokens").unwrap_or(0);
    let cache_read = n("/prompt_tokens_details/cached_tokens")
        .or_else(|| n("/prompt_cache_hit_tokens"))
        .or_else(|| n("/cached_tokens"))
        .unwrap_or(0);
    let cache_write = n("/prompt_tokens_details/cache_write_tokens").unwrap_or(0);
    CallUsage {
        input_tokens: prompt
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output_tokens: n("/completion_tokens").unwrap_or(0),
        thinking_tokens: n("/completion_tokens_details/reasoning_tokens").unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

/// Pi's `mapStopReason`.
fn map_finish_reason(reason: &str) -> Result<StopReason, ClassifiedError> {
    match reason {
        "stop" | "end" => Ok(StopReason::EndTurn),
        "length" => Ok(StopReason::MaxTokens),
        "function_call" | "tool_calls" => Ok(StopReason::ToolUse),
        "network_error" => Err(common::classified(
            ProviderErrorKind::Network,
            None,
            "the provider's finish_reason: network_error".into(),
            ErrorClass::Retryable,
        )),
        other => Err(common::classified(
            ProviderErrorKind::Other,
            None,
            format!("the provider's finish_reason: {other}"),
            ErrorClass::Fatal,
        )),
    }
}

impl StreamParser for ChatCompletionsParser {
    fn feed(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, ClassifiedError> {
        let mut out = Vec::new();
        let data = event.data.trim();
        if self.terminal || data.is_empty() {
            return Ok(out);
        }
        if data == "[DONE]" {
            self.terminal = true;
            return Ok(out);
        }
        let chunk = parse_event_json(data).map_err(|e| common::not_json(&e))?;
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(error));
        }
        if self.response_id.is_none()
            && let Some(id) = chunk.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.response_id = Some(id.to_owned());
        }
        let choice = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first());
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = parse_usage(usage);
        } else if let Some(usage) = choice
            .and_then(|c| c.get("usage"))
            .filter(|u| u.is_object())
        {
            // Some providers (Moonshot) put usage in the choice.
            self.usage = parse_usage(usage);
        }
        let Some(choice) = choice else {
            return Ok(out);
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            match map_finish_reason(reason) {
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
        if let Some(delta) = choice.get("delta").filter(|d| d.is_object()) {
            self.delta(delta, &mut out);
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
        if self.stop.is_none() && !self.supports_finish_reason {
            let has_tools = self
                .blocks
                .iter()
                .any(|b| matches!(b, Partial::ToolCall { .. }));
            self.stop = Some(if has_tools {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            });
        }
        if let Some(error) = self.stop_error.take() {
            return Err(error);
        }
        if self.stop.is_none() {
            return Err(common::classified(
                ProviderErrorKind::Network,
                None,
                "the stream ended without a finish_reason".into(),
                ErrorClass::Retryable,
            ));
        }
        // Blocks end when the stream does, in order (as Pi reports them).
        let mut out: Vec<StreamEvent> = self
            .content()
            .into_iter()
            .map(|block| match block {
                AssistantBlock::Text { .. } => StreamEvent::TextEnd,
                thinking @ AssistantBlock::Thinking { .. } => StreamEvent::ThinkingEnd(thinking),
                call @ AssistantBlock::ToolCall { .. } => StreamEvent::ToolCallEnd(call),
            })
            .collect();
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

/// How errors name the provider.
fn label(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenRouter => "OpenRouter",
        Provider::Kimi => "Kimi",
        Provider::Copilot => "GitHub Copilot",
        _ => "the OpenAI-compatible endpoint",
    }
}

/// `(code, message)` from an OpenAI-style error object; the code may be a string or a number.
fn error_fields(error: &Value) -> (Option<String>, Option<String>) {
    let code = error
        .get("code")
        .and_then(|c| match c {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .or_else(|| error.get("type").and_then(Value::as_str).map(str::to_owned));
    let mut message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| error.as_str().map(str::to_owned));
    // OpenRouter passes the upstream provider's own words here.
    if let Some(raw) = error.pointer("/metadata/raw").and_then(Value::as_str)
        && !message.as_deref().unwrap_or_default().contains(raw)
    {
        message = Some(match message {
            Some(m) => format!("{m} ({raw})"),
            None => raw.to_owned(),
        });
    }
    (code, message)
}

/// Whether an error is the account's plan or credit running out (never retried).
fn is_account_limit(code: Option<&str>, message: Option<&str>) -> bool {
    let code = code.unwrap_or_default().to_ascii_lowercase();
    let message = message.unwrap_or_default().to_ascii_lowercase();
    matches!(
        code.as_str(),
        "insufficient_quota" | "usage_limit_reached" | "quota_exceeded"
    ) || [
        "insufficient_quota",
        "quota",
        "usage limit",
        "insufficient credits",
        "allowance",
    ]
    .iter()
    .any(|m| message.contains(m))
}

/// Classifies a failed HTTP response.
pub fn classify_http_error(
    provider: Provider,
    status: u16,
    headers: &HeaderMap,
    body: &str,
) -> ClassifiedError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().map(|v| v.get("error").unwrap_or(v));
    let (code, message) = error.map_or((None, None), error_fields);
    let message =
        message.or_else(|| (!body.trim().is_empty() && parsed.is_none()).then(|| body.to_owned()));
    let text = format!(
        "{} API error (HTTP {status}): {}",
        label(provider),
        common::error_detail(code.as_deref(), message.as_deref())
    );
    if matches!(status, 402 | 403 | 429) && is_account_limit(code.as_deref(), message.as_deref()) {
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
    let overflow = code.as_deref() == Some("context_length_exceeded")
        || message.as_deref().is_some_and(common::is_context_overflow);
    let classified = common::classify_status(status, text, overflow);
    common::honour_should_retry(classified, header_str(headers, "x-should-retry"))
}

/// An `error` object inside a chunk.
fn stream_error(error: &Value) -> ClassifiedError {
    let (code, message) = error_fields(error);
    let text = format!(
        "the stream reported an error: {}",
        common::error_detail(code.as_deref(), message.as_deref())
    );
    let message_lc = message.as_deref().unwrap_or_default().to_ascii_lowercase();
    if message.as_deref().is_some_and(common::is_context_overflow) {
        return common::classified(
            ProviderErrorKind::ContextOverflow,
            None,
            text,
            ErrorClass::ContextOverflow,
        );
    }
    let code_lc = code.as_deref().unwrap_or_default().to_ascii_lowercase();
    let transient = code_lc.contains("server_error")
        || code_lc.contains("rate_limit")
        || code_lc.starts_with('5')
        || code_lc == "429"
        || message_lc.contains("overloaded")
        || message_lc.contains("try again")
        || message_lc.contains("rate limit")
        || message_lc.contains("service unavailable");
    if transient {
        common::classified(ProviderErrorKind::Server, None, text, ErrorClass::Retryable)
    } else {
        common::classified(ProviderErrorKind::Other, None, text, ErrorClass::Fatal)
    }
}
