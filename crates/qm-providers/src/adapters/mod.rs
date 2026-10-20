//! Wire adapters: one per provider API, behind one trait.
//!
//! An adapter is **pure translation**: it never does I/O, never sleeps and never sees a secret.
//! The engine owns HTTP ([`crate::http`]), retries ([`crate::retry`]), credentials and the replay
//! rules ([`crate::replay`]); the adapter only knows its provider's formats:
//!
//! 1. [`WireAdapter::build_request`] turns a [`ModelRequest`] (our transcript, already prepared by
//!    `replay::prepare`, the tools, the model's profile, the effort) into an [`HttpRequest`]: URL,
//!    headers **without** authorization (the engine adds `Authorization` itself), JSON body.
//! 2. [`WireAdapter::parser`] gives a fresh [`StreamParser`] per attempt. It is fed the response's
//!    SSE events ([`crate::sse`]) and returns [`StreamEvent`]s: text, thinking and tool-call deltas
//!    and ends, and finally `Done` with the whole assistant [`Message`].
//! 3. [`WireAdapter::classify_error`] reads a failed response (status, headers, body) into a
//!    [`ClassifiedError`]: retryable or not, an account limit, an expired login, a context
//!    overflow. The parser reports in-stream errors the same way.
//! 4. [`WireAdapter::normalize_tool_call_id`] maps a tool-call ID from another model to one this
//!    API accepts; the replay rules rename the matching results with it.
//! 5. [`WireAdapter::can_send_output_cap`] says whether the request can carry the orchestrators'
//!    output cap; when it can't, the loop enforces the cap after the call.
//! 6. [`WireAdapter::usage_windows`] reads account usage from response headers, where a provider
//!    sends it.
//!
//! **The shape the other three adapters copy.** Anthropic Messages, Chat Completions and Gemini
//! each get a file next to `responses.rs` with one adapter type implementing [`WireAdapter`], a
//! `WireApi` variant added here, and a match arm in [`adapter_for`]. Each keeps to these rules:
//!
//! - build bodies with `serde_json::json!` from our [`Message`]s; never add a field the provider
//!   rejects for the login in use (e.g. Sign in with ChatGPT and `max_output_tokens`);
//! - keep the provider's opaque replay data (signatures, encrypted reasoning, item IDs) in
//!   `AssistantBlock` signatures as JSON strings (a tool call's in its `provider_signature`),
//!   and replay them only to the same provider, API and model (`replay::prepare` drops them
//!   otherwise);
//! - commit nothing: the parser only reports deltas; the loop commits lines and tool calls;
//! - tolerate the unknown: unknown event types and fields are logged at `debug` and ignored; a
//!   stream that ends without its terminal event, or with a tool call never finished, is an error;
//! - name errors by HTTP status and the provider's error code and message, never by a header
//!   value or anything from the request.

pub mod anthropic;
pub mod chat_completions;
mod common;
pub mod gemini;
pub mod responses;

use crate::model::ModelProfile;
use crate::sse::SseEvent;
use crate::transcript::{AssistantBlock, Message};
use jiff::Timestamp;
use qm_engine::{ProviderError, RateLimitInfo, ToolDescriptor, UsageWindow};
use qm_types::domain::{Effort, Provider};
use serde::{Deserialize, Serialize};
use std::fmt;

pub use reqwest::header::HeaderMap;

/// A provider API, as recorded on every assistant message (replay rules compare it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireApi {
    /// The OpenAI Responses API: at `api.openai.com/v1` (Sign in with ChatGPT, an OpenAI API key)
    /// and xAI's at `api.x.ai/v1`. The message's provider tells the two apart.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// OpenAI Responses on the ChatGPT backend (`chatgpt.com/backend-api/codex`, the Codex login).
    #[serde(rename = "openai_codex_responses")]
    OpenAiCodexResponses,
    /// Anthropic Messages at `api.anthropic.com` (an Anthropic API key).
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    /// OpenAI Chat Completions with per-provider compatibility flags (OpenRouter, Kimi, Copilot,
    /// custom OpenAI-compatible endpoints).
    #[serde(rename = "openai_completions")]
    OpenAiCompletions,
    /// Gemini's `streamGenerateContent` at `generativelanguage.googleapis.com`.
    #[serde(rename = "google_generative_ai")]
    GoogleGenerativeAi,
}

impl WireApi {
    /// The name in the init report and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiCodexResponses => "openai-codex-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiCompletions => "openai-completions",
            Self::GoogleGenerativeAi => "google-generative-ai",
        }
    }
}

impl fmt::Display for WireApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of credential a request will carry: it decides the endpoint and which fields are
/// allowed. Never the secret itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// A pay-per-token API key (`sk-…`).
    ApiKey,
    /// A Sign in with ChatGPT access token: straight to the Responses API, which rejects
    /// `max_output_tokens`, `temperature`, `prompt_cache_retention` and `configuration_update`.
    SignInWithChatGpt,
    /// A Codex-login token: the ChatGPT backend, with the account's ID in a header.
    Codex { account_id: String },
}

/// How the credential goes on a request. The engine adds it (the adapter never sees the secret);
/// most APIs take a bearer token, but Anthropic and Gemini API keys go in a header of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialHeader {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `<name>: <secret>` (e.g. `x-api-key`, `x-goog-api-key`).
    Named(&'static str),
}

/// Everything an adapter needs to build one request.
#[derive(Clone, Copy, Debug)]
pub struct ModelRequest<'a> {
    pub provider: Provider,
    pub model: &'a ModelProfile,
    pub effort: Effort,
    /// The whole system prompt (always ours).
    pub system_prompt: &'a str,
    /// The context, already prepared by `replay::prepare` for this target.
    pub messages: &'a [Message],
    /// Every tool offered: ours and the built-in ones.
    pub tools: &'a [ToolDescriptor],
    /// The session's ID: the prompt-cache key and the request-correlation headers.
    pub session_id: &'a str,
    /// The orchestrators' output cap, sent where the API allows it.
    pub max_output_tokens: Option<u32>,
    pub credential: &'a CredentialKind,
    /// A custom base URL (`AccountRef.base_url`), else the provider's.
    pub base_url: Option<&'a str>,
    pub user_agent: &'a str,
}

/// A request ready to send, without authorization.
#[derive(Clone, Debug, PartialEq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

/// Why a request couldn't be built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("{0}")]
    Invalid(String),
}

/// What the loop should do about an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Worth another attempt (if nothing was committed and retries are left).
    Retryable,
    /// Never retried.
    Fatal,
    /// The account hit its usage limit: an account limit, never retried.
    AccountLimit {
        window: Option<String>,
        resets_at: Option<Timestamp>,
    },
    /// The login was refused: refresh once and repeat.
    Unauthorized,
    /// The context is too long for the model: compact once and repeat.
    ContextOverflow,
}

/// An error with its class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedError {
    pub error: ProviderError,
    pub class: ErrorClass,
}

impl ClassifiedError {
    pub fn new(error: ProviderError, class: ErrorClass) -> Self {
        Self { error, class }
    }
}

/// What a parser reports, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    /// The text block ended (the loop flushes its last partial line).
    TextEnd,
    ThinkingDelta(String),
    ThinkingEnd(AssistantBlock),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta(String),
    ToolCallEnd(AssistantBlock),
    /// An in-stream rate-limit report (none of the Responses flavours sends one yet).
    RateLimit(RateLimitInfo),
    /// The reply is complete.
    Done {
        message: Message,
    },
}

/// Parses one response's SSE events. Pure; one per attempt.
pub trait StreamParser: Send + fmt::Debug {
    /// Takes one SSE event. An error is the stream reporting a failure (an `error` event, a failed
    /// response): the call failed.
    fn feed(&mut self, event: &SseEvent) -> Result<Vec<StreamEvent>, ClassifiedError>;

    /// Whether the terminal event has been seen (the loop may stop reading).
    fn is_terminal(&self) -> bool;

    /// The stream ended: `Done` with the whole message, or an error if the reply is incomplete
    /// (no terminal event, an unfinished tool call).
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ClassifiedError>;

    /// What has streamed so far, as an assistant message marked aborted (an interrupt or a
    /// failure mid-call): written to the session file, never replayed.
    fn partial_message(&self) -> Message;
}

/// One provider API.
pub trait WireAdapter: Send + Sync + fmt::Debug {
    fn api(&self) -> WireApi;

    /// URL, headers (no authorization) and JSON body.
    fn build_request(&self, req: &ModelRequest<'_>) -> Result<HttpRequest, AdapterError>;

    /// A fresh parser for one attempt of `req`.
    fn parser(&self, req: &ModelRequest<'_>) -> Box<dyn StreamParser>;

    /// Reads a failed HTTP response.
    fn classify_error(&self, status: u16, headers: &HeaderMap, body: &str) -> ClassifiedError;

    /// A tool-call ID written by another model, made acceptable to this API.
    /// `from_foreign_model` is true when it came from another provider or API.
    fn normalize_tool_call_id(&self, id: &str, from_foreign_model: bool) -> String;

    /// Whether `req` may carry `max_output_tokens`.
    fn can_send_output_cap(&self, req: &ModelRequest<'_>) -> bool;

    /// Account usage windows from response headers, where the provider sends them.
    fn usage_windows(&self, _headers: &HeaderMap, _now: Timestamp) -> Vec<UsageWindow> {
        Vec::new()
    }

    /// Where the credential goes on this API's requests: a bearer token unless the adapter says
    /// otherwise. The engine's HTTP client must honour it.
    fn credential_header(&self, _credential: &CredentialKind) -> CredentialHeader {
        CredentialHeader::Bearer
    }
}

/// The adapter for an account's provider and credential, or `None` for a provider the engine
/// doesn't serve (Claude subscriptions run on the Claude engine, the scripted engine on its own).
pub fn adapter_for(
    provider: Provider,
    credential: &CredentialKind,
) -> Option<Box<dyn WireAdapter>> {
    match provider {
        Provider::ChatGpt | Provider::OpenAi => Some(Box::new(match credential {
            CredentialKind::Codex { .. } => responses::ResponsesAdapter::codex(),
            CredentialKind::ApiKey | CredentialKind::SignInWithChatGpt => {
                responses::ResponsesAdapter::platform()
            }
        })),
        // xAI's Responses API honours `reasoning.effort`; its Chat Completions doesn't.
        Provider::Xai => Some(Box::new(responses::ResponsesAdapter::xai())),
        Provider::Anthropic => Some(Box::new(anthropic::AnthropicAdapter::new())),
        Provider::Gemini => Some(Box::new(gemini::GeminiAdapter::new())),
        Provider::OpenRouter | Provider::Kimi | Provider::Copilot | Provider::Custom => Some(
            Box::new(chat_completions::ChatCompletionsAdapter::new(provider)),
        ),
        Provider::Claude | Provider::Scripted => None,
    }
}

/// Whether the engine can serve this provider at all: every provider but Claude subscriptions
/// (the Claude engine) and the scripted engine.
pub const fn provider_supported(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::ChatGpt
            | Provider::OpenAi
            | Provider::Anthropic
            | Provider::Gemini
            | Provider::Xai
            | Provider::OpenRouter
            | Provider::Kimi
            | Provider::Copilot
            | Provider::Custom
    )
}
