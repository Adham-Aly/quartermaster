//! One turn: call the model, run the tools it asked for, send back the results, repeat, after Pi's
//! `packages/agent/src/agent-loop.ts` at the pinned commit (MIT).
//!
//! ```text
//! turn(input):
//!   append the user message; TurnStarted
//!   compact first if the context is over the threshold
//!   loop:
//!     take steering (injected) messages into the context
//!     n += 1; ModelCallStarted{n}
//!     message = call_with_retries(context)          streams text lines, Activity
//!     cancelled → write the partial reply as aborted; TurnEnded{Interrupted}
//!     failed    → TurnEnded{Failed{…}}
//!     ModelCallFinished{n, usage, stop}; append the reply
//!     the output cap passed → TurnEnded{Failed{Provider}}
//!     MaxTokens with tool calls → each call gets an error result (its arguments may be cut off)
//!     else run the calls concurrently, results kept in call order:
//!       ours (spec.tools) → ToolCall, then wait for the core's ToolResult
//!       built-in          → BuiltinToolUse; run here; Activity{BuiltinTool} while it runs
//!       unknown           → error result "unknown tool <name>"
//!     no calls and no steering → done
//!   TurnEnded{Completed, usage, final_text}
//! ```
//!
//! **Exact counters.** One `ModelCallStarted` per call, never per attempt; one
//! `ModelCallFinished` per successful call with its usage; `ToolCall` immediately before waiting,
//! so the core's `TurnMeter` pauses the call clock exactly while our tools run. A compaction's
//! summary request is not the agent's call and reports no `ModelCall*`.
//!
//! **Retries.** Per call, `limits.max_retries` (2 by default) with `Retrying` before each wait;
//! never after a `TextLine` of that call was committed; a usage limit is `RateLimit { Rejected }`
//! then `Failed { AccountLimit }`; a 401 refreshes the login once and repeats without counting a
//! retry; a context overflow compacts once and repeats.
//!
//! **Cancellation.** Dropping the request future aborts it; running built-in calls are dropped
//! (which cancels them); pending calls to our tools get "interrupted" results in the transcript.

use crate::adapters::{
    ClassifiedError, CredentialHeader, CredentialKind, ErrorClass, HeaderMap, HttpRequest,
    ModelRequest, StreamEvent, StreamParser, WireAdapter,
};
use crate::builtin::{BuiltinOutcome, JobNotice, summarize_args};
use crate::compaction::{self, KEEP_RECENT_TOKENS, ON_REQUEST_KEEP_TOKENS, SUMMARY_SYSTEM_PROMPT};
use crate::replay::{self, ReplayTarget};
use crate::retry::NoRetry;
use crate::session::{Cancel, Session};
use crate::session_store::Entry;
use crate::sse::SseFramer;
use crate::transcript::{Block, Message, args_are_valid};
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use qm_auth::Secret;
use qm_engine::{
    ActivityKind, AutocompactPoint, CallUsage, ContextBasis, ContextReading, EngineEvent,
    FailureKind, InputBlock, LineBuffer, ProviderError, ProviderErrorKind, RateLimitInfo,
    RateLimitStatus, StopReason, ToolCallId, TurnFailure, TurnInput, TurnOutcome, TurnUsage,
};
use std::sync::Arc;
use std::time::Duration;

/// A stream that sends nothing for this long is dead.
const STREAM_IDLE_LIMIT: Duration = Duration::from_mins(10);

/// The result text of a call answered without running, when the reply hit the output limit.
pub const TRUNCATED_CALL: &str =
    "not executed: the reply hit the output limit, so its arguments may be truncated";
/// The result text of a call whose arguments didn't parse.
pub const INVALID_ARGS: &str = "arguments were not valid JSON";
/// The result text of a pending call when the turn was interrupted.
pub const INTERRUPTED: &str = "interrupted";
/// A 401 after a refresh.
pub const LOGIN_EXPIRED: &str = "the login has expired; connect it again in Settings ▸ Accounts";

/// How a model call ended.
enum CallOutcome {
    Done(Message),
    Cancelled(Cancel, Option<Message>),
    Failed(TurnFailure),
}

/// How one attempt ended.
enum AttemptError {
    Cancelled(Cancel, Option<Message>),
    Failed(Box<FailedAttempt>),
}

/// A failed attempt.
struct FailedAttempt {
    error: ClassifiedError,
    headers: Option<HeaderMap>,
    /// A text line of this call was already committed: no retry.
    committed: bool,
    /// What streamed before the failure.
    partial: Option<Message>,
}

impl AttemptError {
    fn failed(
        error: ClassifiedError,
        headers: Option<HeaderMap>,
        committed: bool,
        partial: Option<Message>,
    ) -> Self {
        Self::Failed(Box::new(FailedAttempt {
            error,
            headers,
            committed,
            partial,
        }))
    }

    /// A failure that no retry would fix (an unbuildable request).
    fn fatal(message: String) -> Self {
        Self::failed(
            ClassifiedError::new(
                ProviderError {
                    kind: ProviderErrorKind::Other,
                    status: None,
                    message,
                },
                ErrorClass::Fatal,
            ),
            None,
            false,
            None,
        )
    }
}

/// What one call's attempts have used so far.
#[derive(Default)]
struct RetryState {
    /// Retries used (`Retrying` events sent).
    retries: u32,
    /// The token a 401 refused, to refresh past it.
    refused: Option<Secret>,
    /// The one refresh after a 401 was used.
    refreshed: bool,
    /// The one compaction after a context overflow was used.
    overflow_compacted: bool,
}

/// What a call is made of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallMode {
    /// The agent's call: the context and the tools; text is committed as lines.
    Agent,
    /// A compaction summary: the given prompt, no tools, nothing committed, no `Retrying`.
    Summary,
}

/// How a turn ends.
struct TurnEnd {
    outcome: TurnOutcome,
    final_text: String,
}

/// The turn's running totals.
#[derive(Default)]
struct Totals {
    calls: u32,
    usage: TurnUsage,
}

impl Totals {
    fn add(&mut self, usage: &CallUsage) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
        self.usage.thinking_tokens += usage.thinking_tokens;
        self.usage.cache_read_tokens += usage.cache_read_tokens;
        self.usage.cache_write_tokens += usage.cache_write_tokens;
    }
}

impl Session {
    /// Runs one turn to its end (always exactly one `TurnStarted` and one `TurnEnded`).
    pub(crate) async fn run_turn(&mut self, input: TurnInput) {
        let started = self.shared.clock.instant();
        let user = user_message(&input).await;
        self.record(user);
        self.emit(EngineEvent::TurnStarted).await;
        let mut totals = Totals::default();
        let end = self.turn_body(&mut totals).await;
        let mut usage = totals.usage;
        usage.model_calls = totals.calls;
        // Helper agents' calls count toward the crewmate's turn.
        if let Some(tools) = &self.builtins {
            crate::helpers::add_usage(&mut usage, &tools.take_usage());
        }
        usage.duration_ms = u64::try_from(
            self.shared
                .clock
                .instant()
                .saturating_duration_since(started)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let outcome_name = match &end.outcome {
            TurnOutcome::Completed => "completed",
            TurnOutcome::Interrupted => "interrupted",
            TurnOutcome::Failed { .. } => "failed",
        };
        self.pending_results.clear();
        self.emit(EngineEvent::TurnEnded {
            outcome: end.outcome,
            usage,
            final_text: end.final_text,
        })
        .await;
        let id = self.history.next_id();
        let at = self.shared.clock.now();
        let entry = Entry::TurnEnd {
            id,
            at,
            outcome: outcome_name.to_owned(),
        };
        if let Err(e) = self.history.write(&entry) {
            tracing::warn!(error = %e, "couldn't finish the session history's turn");
        }
    }

    async fn turn_body(&mut self, totals: &mut Totals) -> TurnEnd {
        let mut final_text = String::new();
        loop {
            for input in std::mem::take(&mut self.steering) {
                let message = user_message(&input).await;
                self.record(message);
            }
            match self.compact_if_due().await {
                Ok(()) | Err(CompactError::Nothing) => {}
                Err(CompactError::Failed(reason)) => {
                    tracing::warn!(%reason, "compaction before a model call failed; continuing");
                }
                Err(CompactError::Cancelled(cancel)) => return interrupted(cancel, final_text),
            }
            totals.calls += 1;
            let call = totals.calls;
            self.emit(EngineEvent::ModelCallStarted { call }).await;
            let message = match self.call_model(CallMode::Agent, None).await {
                CallOutcome::Done(message) => message,
                CallOutcome::Cancelled(cancel, partial) => {
                    if let Some(partial) = partial.filter(has_content) {
                        self.record(partial);
                    }
                    return interrupted(cancel, final_text);
                }
                CallOutcome::Failed(failure) => {
                    return TurnEnd {
                        outcome: TurnOutcome::Failed { failure },
                        final_text,
                    };
                }
            };
            let (usage, stop) = match &message {
                Message::Assistant { usage, stop, .. } => (*usage, *stop),
                _ => (CallUsage::default(), StopReason::Unknown),
            };
            self.emit(EngineEvent::ModelCallFinished { call, usage, stop })
                .await;
            // Passive: this call's own prompt plus reply, the size the next request re-sends.
            let tokens = usage.input_tokens
                + usage.cache_read_tokens
                + usage.cache_write_tokens
                + usage.output_tokens;
            let reading = self.context_reading(Some(tokens), ContextBasis::LastCall);
            self.emit(EngineEvent::ContextObserved { reading }).await;
            totals.add(&usage);
            final_text = message.text();
            let calls: Vec<(String, String, serde_json::Value)> = message
                .tool_calls()
                .into_iter()
                .map(|(id, name, args)| (id.to_owned(), name.to_owned(), args.clone()))
                .collect();
            self.record(message);
            if let Some(failure) = self.limits.cap_failure(&usage, stop) {
                return TurnEnd {
                    outcome: TurnOutcome::Failed { failure },
                    final_text,
                };
            }
            if !calls.is_empty() {
                if stop == StopReason::MaxTokens {
                    for (id, name, _) in &calls {
                        self.record(Message::ToolResult {
                            call_id: id.clone(),
                            name: name.clone(),
                            content: vec![Block::text(TRUNCATED_CALL)],
                            is_error: true,
                        });
                    }
                } else if let Err(cancel) = self.run_tools(&calls).await {
                    return interrupted(cancel, final_text);
                }
            }
            if calls.is_empty() && self.steering.is_empty() {
                break;
            }
        }
        TurnEnd {
            outcome: TurnOutcome::Completed,
            final_text,
        }
    }

    /// Runs one assistant message's tool calls; appends their results in call order.
    async fn run_tools(
        &mut self,
        calls: &[(String, String, serde_json::Value)],
    ) -> Result<(), Cancel> {
        let mut results: Vec<Option<Message>> = vec![None; calls.len()];
        let mut ours: Vec<(usize, String)> = Vec::new();
        let mut builtin_calls: Vec<(usize, String, serde_json::Value)> = Vec::new();
        let builtins = self.builtins.clone();
        for (index, (id, name, args)) in calls.iter().enumerate() {
            if !args_are_valid(args) {
                results[index] = Some(error_result(id, name, INVALID_ARGS));
            } else if self.our_tools.contains(name) {
                ours.push((index, id.clone()));
            } else if builtins.as_ref().is_some_and(|b| b.handles(name)) {
                builtin_calls.push((index, name.clone(), args.clone()));
            } else {
                results[index] = Some(error_result(id, name, &format!("unknown tool {name}")));
            }
        }
        // Built-in calls are announced and started; ours go to the core.
        for (_, name, args) in &builtin_calls {
            self.emit(EngineEvent::BuiltinToolUse {
                name: name.clone(),
                summary: summarize_args(name, args),
            })
            .await;
        }
        for (index, id) in &ours {
            let (_, name, args) = &calls[*index];
            self.pending_results.insert(id.clone(), None);
            self.emit(EngineEvent::ToolCall {
                call: ToolCallId::new(id.clone()),
                name: name.clone(),
                args: args.clone(),
            })
            .await;
        }
        let outcome = {
            let mut running: FuturesUnordered<_> = builtin_calls
                .iter()
                .filter_map(|(index, name, args)| {
                    let tools = builtins.as_ref()?;
                    let fut = tools.run(name, args.clone());
                    Some(async move { (*index, fut.await) })
                })
                .collect();
            let mut outcome = Ok(());
            loop {
                let ours_done = ours
                    .iter()
                    .all(|(_, id)| matches!(self.pending_results.get(id), Some(Some(_))));
                if ours_done && running.is_empty() {
                    break;
                }
                let activity = (!running.is_empty()).then_some(ActivityKind::BuiltinTool);
                let next = async {
                    if running.is_empty() {
                        std::future::pending().await
                    } else {
                        running.next().await
                    }
                };
                match self.wait_or_command(next, activity).await {
                    Ok(Some(Some((index, done)))) => {
                        let (id, name, _) = &calls[index];
                        results[index] = Some(self.builtin_result(id, name, done).await);
                    }
                    Ok(Some(None) | None) => {}
                    Err(cancel) => {
                        outcome = Err(cancel);
                        break;
                    }
                }
            }
            outcome
        };
        for (index, id) in &ours {
            let (_, name, _) = &calls[*index];
            let message = match self.pending_results.remove(id).flatten() {
                Some(result) => Message::ToolResult {
                    call_id: id.clone(),
                    name: name.clone(),
                    content: vec![Block::text(result.text)],
                    is_error: result.is_error,
                },
                None => error_result(id, name, INTERRUPTED),
            };
            results[*index] = Some(message);
        }
        for (index, result) in results.into_iter().enumerate() {
            let (id, name, _) = &calls[index];
            self.record(result.unwrap_or_else(|| error_result(id, name, INTERRUPTED)));
        }
        outcome
    }

    async fn builtin_result(&mut self, id: &str, name: &str, done: BuiltinOutcome) -> Message {
        if let Some(JobNotice::Started {
            job,
            command,
            log_path,
        }) = done.job_started
        {
            self.start_job(job, command, log_path).await;
        }
        let mut content = vec![Block::text(done.text)];
        content.extend(
            done.images
                .into_iter()
                .map(|(media_type, bytes)| Block::Image { media_type, bytes }),
        );
        Message::ToolResult {
            call_id: id.to_owned(),
            name: name.to_owned(),
            content,
            is_error: done.is_error,
        }
    }

    /// One model call with its retries, refreshes and overflow compaction. `fixed` is the summary
    /// request's prompt in [`CallMode::Summary`]. Boxed with an explicit `Send`: a compaction
    /// inside makes a call of its own.
    fn call_model<'a>(
        &'a mut self,
        mode: CallMode,
        fixed: Option<&'a str>,
    ) -> qm_types::BoxFuture<'a, CallOutcome> {
        Box::pin(async move {
            let mut state = RetryState::default();
            loop {
                let (secret, kind) = match self
                    .shared
                    .credential(&self.spec.account, state.refused.as_ref())
                    .await
                {
                    Ok(c) => c,
                    Err(message) => return CallOutcome::Failed(provider_failure(message)),
                };
                match self.attempt(mode, fixed, &secret, &kind).await {
                    Ok(message) => return CallOutcome::Done(message),
                    Err(AttemptError::Cancelled(cancel, partial)) => {
                        return CallOutcome::Cancelled(cancel, partial);
                    }
                    Err(AttemptError::Failed(failed)) => {
                        if let Some(outcome) = self
                            .after_failure(mode, *failed, secret, &kind, &mut state)
                            .await
                        {
                            return outcome;
                        }
                    }
                }
            }
        })
    }

    /// Decides what a failed attempt leads to: `None` to try again (after a refresh, a
    /// compaction or a back-off), or how the call ends.
    async fn after_failure(
        &mut self,
        mode: CallMode,
        failed: FailedAttempt,
        secret: Secret,
        kind: &CredentialKind,
        state: &mut RetryState,
    ) -> Option<CallOutcome> {
        let FailedAttempt {
            error,
            headers,
            committed,
            partial,
        } = failed;
        if mode == CallMode::Agent
            && let Some(mut partial) = partial.filter(has_content)
        {
            if let Message::Assistant {
                aborted, failed, ..
            } = &mut partial
            {
                *aborted = false;
                *failed = true;
            }
            self.record(partial);
        }
        match error.class {
            ErrorClass::Unauthorized if !state.refreshed && *kind != CredentialKind::ApiKey => {
                state.refreshed = true;
                state.refused = Some(secret);
                None
            }
            ErrorClass::Unauthorized => {
                let problem = if *kind == CredentialKind::ApiKey {
                    format!("the API key was refused: {}", error.error.message)
                } else {
                    LOGIN_EXPIRED.to_owned()
                };
                self.shared.login_refused(&self.spec.account, &problem);
                Some(CallOutcome::Failed(provider_failure(problem)))
            }
            ErrorClass::AccountLimit { window, resets_at } => {
                self.emit(EngineEvent::RateLimit {
                    info: RateLimitInfo {
                        status: RateLimitStatus::Rejected,
                        window,
                        resets_at,
                    },
                })
                .await;
                Some(CallOutcome::Failed(TurnFailure {
                    kind: FailureKind::AccountLimit,
                    message: error.error.message,
                }))
            }
            ErrorClass::ContextOverflow
                if mode == CallMode::Agent && !state.overflow_compacted && !committed =>
            {
                state.overflow_compacted = true;
                match self.compact().await {
                    Ok(()) => None,
                    Err(CompactError::Cancelled(cancel)) => {
                        Some(CallOutcome::Cancelled(cancel, None))
                    }
                    Err(CompactError::Nothing) => {
                        Some(CallOutcome::Failed(provider_failure(error.error.message)))
                    }
                    Err(CompactError::Failed(reason)) => {
                        Some(CallOutcome::Failed(provider_failure(format!(
                            "{} (compacting the conversation failed: {reason})",
                            error.error.message
                        ))))
                    }
                }
            }
            ErrorClass::Retryable if !committed => {
                self.back_off(mode, &error, headers.as_ref(), state).await
            }
            _ => Some(CallOutcome::Failed(provider_failure(error.error.message))),
        }
    }

    /// Waits before the next retry (reporting `Retrying` for the agent's calls), or ends the call
    /// when no retry is left.
    async fn back_off(
        &mut self,
        mode: CallMode,
        error: &ClassifiedError,
        headers: Option<&HeaderMap>,
        state: &mut RetryState,
    ) -> Option<CallOutcome> {
        let now = self.shared.clock.now();
        match self.retry.delay(state.retries, headers, now) {
            Ok(delay) => {
                state.retries += 1;
                if mode == CallMode::Agent {
                    self.emit(EngineEvent::Retrying {
                        attempt: state.retries,
                        max_attempts: self.retry.max_retries(),
                        error: error.error.clone(),
                        delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    })
                    .await;
                }
                let sleep = self.shared.clock.sleep(delay);
                match self.wait(sleep, None).await {
                    Ok(()) => None,
                    Err(cancel) => Some(CallOutcome::Cancelled(cancel, None)),
                }
            }
            Err(NoRetry::Exhausted) => Some(CallOutcome::Failed(provider_failure(
                if state.retries == 0 {
                    error.error.message.clone()
                } else {
                    format!(
                        "the provider kept failing after {} retries: {}",
                        state.retries, error.error.message
                    )
                },
            ))),
            Err(NoRetry::ServerDelayTooLong(d)) => {
                Some(CallOutcome::Failed(provider_failure(format!(
                    "the provider asked to wait {} s before retrying (more than 60 s): {}",
                    d.as_secs(),
                    error.error.message
                ))))
            }
        }
    }

    /// One HTTP attempt: build, send, stream, parse.
    async fn attempt(
        &mut self,
        mode: CallMode,
        fixed: Option<&str>,
        secret: &Secret,
        kind: &CredentialKind,
    ) -> Result<Message, AttemptError> {
        let Some(adapter) = self.adapter_for(kind) else {
            return Err(AttemptError::fatal(format!(
                "the provider engine has no adapter for {} accounts yet",
                self.spec.account.provider
            )));
        };
        let raw_messages = match (mode, fixed) {
            (CallMode::Summary, Some(prompt)) => vec![Message::user_text(prompt)],
            _ => self.context.messages(),
        };
        let target = ReplayTarget {
            provider: self.spec.account.provider,
            api: adapter.api(),
            model: &self.model.id,
            image_input: self.model.image_input,
        };
        let messages = replay::prepare(&raw_messages, target, adapter.as_ref());
        let (system_prompt, tools, cap) = match mode {
            CallMode::Agent => (
                self.system_prompt.as_str(),
                self.tools.as_slice(),
                self.limits.output_cap,
            ),
            CallMode::Summary => (SUMMARY_SYSTEM_PROMPT, &[][..], None),
        };
        let req = ModelRequest {
            provider: self.spec.account.provider,
            model: &self.model,
            effort: self.spec.effort,
            system_prompt,
            messages: &messages,
            tools,
            session_id: &self.id,
            max_output_tokens: cap,
            credential: kind,
            base_url: self.spec.account.base_url.as_deref(),
            user_agent: &self.shared.user_agent,
        };
        let request = adapter
            .build_request(&req)
            .map_err(|e| AttemptError::fatal(e.to_string()))?;
        let parser = adapter.parser(&req);
        let thinking = (mode == CallMode::Agent || self.in_turn).then_some(ActivityKind::Thinking);
        let placement = adapter.credential_header(kind);
        let response = self
            .send(&request, (secret, placement), adapter.as_ref(), thinking)
            .await?;
        self.read_stream(response, &request.url, parser, mode, thinking)
            .await
    }

    /// Sends the request; a non-2xx response becomes a classified failure.
    async fn send(
        &mut self,
        request: &HttpRequest,
        credential: (&Secret, CredentialHeader),
        adapter: &dyn WireAdapter,
        thinking: Option<ActivityKind>,
    ) -> Result<reqwest::Response, AttemptError> {
        let shared = Arc::clone(&self.shared);
        let response = match self
            .wait(shared.http.send(request, Some(credential)), thinking)
            .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                return Err(AttemptError::failed(
                    ClassifiedError::new(error, ErrorClass::Retryable),
                    None,
                    false,
                    None,
                ));
            }
            Err(cancel) => return Err(AttemptError::Cancelled(cancel, None)),
        };
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let now = self.shared.clock.now();
        let windows = adapter.usage_windows(&headers, now);
        self.report_usage_windows(&windows).await;
        self.shared.record_usage(self.spec.account.id, windows, now);
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let body = match self.wait(response.text(), thinking).await {
            Ok(text) => text.unwrap_or_default(),
            Err(cancel) => return Err(AttemptError::Cancelled(cancel, None)),
        };
        Err(AttemptError::failed(
            adapter.classify_error(status, &headers, &body),
            Some(headers),
            false,
            None,
        ))
    }

    /// Reads the SSE stream through the parser, committing text lines as they complete.
    async fn read_stream(
        &mut self,
        response: reqwest::Response,
        url: &str,
        mut parser: Box<dyn StreamParser>,
        mode: CallMode,
        thinking: Option<ActivityKind>,
    ) -> Result<Message, AttemptError> {
        let shared = Arc::clone(&self.shared);
        let mut stream = response.bytes_stream();
        let mut framer = SseFramer::new();
        let mut lines = LineBuffer::default();
        let mut committed = false;
        let fail = |error: ClassifiedError, committed: bool, parser: &dyn StreamParser| {
            AttemptError::failed(error, None, committed, Some(parser.partial_message()))
        };
        loop {
            let idle = shared.clock.sleep(STREAM_IDLE_LIMIT);
            let next = async {
                tokio::select! {
                    chunk = stream.next() => Some(chunk),
                    () = idle => None,
                }
            };
            let chunk: Option<Bytes> = match self.wait(next, thinking).await {
                Err(cancel) => {
                    return Err(AttemptError::Cancelled(
                        cancel,
                        Some(parser.partial_message()),
                    ));
                }
                Ok(None) => {
                    let error = ClassifiedError::new(
                        ProviderError {
                            kind: ProviderErrorKind::Network,
                            status: None,
                            message: "the response stream sent nothing for 10 minutes".into(),
                        },
                        ErrorClass::Retryable,
                    );
                    return Err(fail(error, committed, parser.as_ref()));
                }
                Ok(Some(None)) => None,
                Ok(Some(Some(Ok(bytes)))) => Some(bytes),
                Ok(Some(Some(Err(e)))) => {
                    let error = ClassifiedError::new(
                        crate::http::network_error(url, &e),
                        ErrorClass::Retryable,
                    );
                    return Err(fail(error, committed, parser.as_ref()));
                }
            };
            let events = match &chunk {
                Some(bytes) => framer.push(bytes),
                None => framer.finish().into_iter().collect(),
            };
            for event in events {
                let parsed = parser
                    .feed(&event)
                    .map_err(|error| fail(error, committed, parser.as_ref()))?;
                if let Some(message) = self
                    .handle_stream_events(parsed, mode, &mut lines, &mut committed)
                    .await
                {
                    return Ok(message);
                }
                if parser.is_terminal() {
                    break;
                }
            }
            if chunk.is_none() || parser.is_terminal() {
                let finished = parser
                    .finish()
                    .map_err(|error| fail(error, committed, parser.as_ref()))?;
                if let Some(message) = self
                    .handle_stream_events(finished, mode, &mut lines, &mut committed)
                    .await
                {
                    return Ok(message);
                }
                let error = ClassifiedError::new(
                    ProviderError {
                        kind: ProviderErrorKind::Other,
                        status: None,
                        message: "the response ended without a reply".into(),
                    },
                    ErrorClass::Retryable,
                );
                return Err(fail(error, committed, parser.as_ref()));
            }
        }
    }

    /// Commits text lines and reports silent work; returns the reply on `Done`.
    async fn handle_stream_events(
        &mut self,
        events: Vec<StreamEvent>,
        mode: CallMode,
        lines: &mut LineBuffer,
        committed: &mut bool,
    ) -> Option<Message> {
        for event in events {
            match event {
                StreamEvent::TextDelta(delta) => {
                    if mode == CallMode::Agent {
                        for line in lines.push(&delta) {
                            *committed = true;
                            self.emit(EngineEvent::TextLine { text: line }).await;
                        }
                    } else {
                        self.activity(ActivityKind::Thinking).await;
                    }
                }
                StreamEvent::TextEnd => {
                    if mode == CallMode::Agent
                        && let Some(line) = lines.flush()
                    {
                        *committed = true;
                        self.emit(EngineEvent::TextLine { text: line }).await;
                    }
                }
                StreamEvent::ThinkingDelta(_)
                | StreamEvent::ThinkingEnd(_)
                | StreamEvent::ToolCallStart { .. }
                | StreamEvent::ToolCallArgsDelta(_)
                | StreamEvent::ToolCallEnd(_) => self.activity(ActivityKind::Thinking).await,
                StreamEvent::RateLimit(info) => {
                    self.emit(EngineEvent::RateLimit { info }).await;
                }
                StreamEvent::Done { message } => {
                    if mode == CallMode::Agent
                        && let Some(line) = lines.flush()
                    {
                        *committed = true;
                        self.emit(EngineEvent::TextLine { text: line }).await;
                    }
                    return Some(message);
                }
            }
        }
        None
    }

    /// Reports usage windows whose status changed since the last report.
    async fn report_usage_windows(&mut self, windows: &[qm_engine::UsageWindow]) {
        for window in windows {
            let status = if window.used_percent >= 100.0 {
                RateLimitStatus::Rejected
            } else if window.used_percent >= 80.0 {
                RateLimitStatus::AllowedWarning
            } else {
                RateLimitStatus::Allowed
            };
            if self.rate_limits.get(&window.name) == Some(&status) {
                continue;
            }
            self.rate_limits.insert(window.name.clone(), status);
            self.emit(EngineEvent::RateLimit {
                info: RateLimitInfo {
                    status,
                    window: Some(window.name.clone()),
                    resets_at: window.resets_at,
                },
            })
            .await;
        }
    }

    /// Compacts the context (see [`crate::compaction`]). Mid-turn the summary request shows as
    /// `Activity { Thinking }`; it reports no `ModelCall*`.
    pub(crate) async fn compact(&mut self) -> Result<(), CompactError> {
        self.compact_keeping(KEEP_RECENT_TOKENS, true).await
    }

    /// The automatic check, before each model call of a turn and between turns: a token count
    /// against the threshold, with the repeat guard. It calls the model only when it compacts. The
    /// autocompact window is read from the settings now, so a change applies at this check.
    pub(crate) async fn compact_if_due(&mut self) -> Result<(), CompactError> {
        let tokens = compaction::context_tokens(&self.context);
        // Copied out first: the settings' read guard is not held across the compaction.
        let setting = u32::from(self.shared.settings.borrow().compaction.autocompact_window);
        let threshold = compaction::threshold(self.model.context_window, setting);
        if !compaction::auto_compaction_due(tokens, threshold, self.compaction_guard) {
            return Ok(());
        }
        let result = self.compact().await;
        if let Err(CompactError::Failed(_)) = result {
            // Not retried on every call: the context must grow past this size first.
            self.compaction_guard = Some(tokens);
        }
        result
    }

    /// A compaction you asked for (`SessionCommand::Compact`), as a turn of its own: `TurnStarted`,
    /// `Compacted` if anything was summarised, `TurnEnded` (rule 7 of the event order).
    pub(crate) async fn run_compaction(&mut self) {
        let started = self.shared.clock.instant();
        self.emit(EngineEvent::TurnStarted).await;
        let outcome = match self.compact_keeping(ON_REQUEST_KEEP_TOKENS, false).await {
            Ok(()) | Err(CompactError::Nothing) => TurnOutcome::Completed,
            Err(CompactError::Failed(reason)) => TurnOutcome::Failed {
                failure: provider_failure(reason),
            },
            Err(CompactError::Cancelled(_)) => TurnOutcome::Interrupted,
        };
        let usage = TurnUsage {
            duration_ms: u64::try_from(
                self.shared
                    .clock
                    .instant()
                    .saturating_duration_since(started)
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            ..TurnUsage::default()
        };
        self.emit(EngineEvent::TurnEnded {
            outcome,
            usage,
            final_text: String::new(),
        })
        .await;
    }

    /// Summarises all but about the newest `keep_tokens` of the context.
    async fn compact_keeping(
        &mut self,
        keep_tokens: u64,
        automatic: bool,
    ) -> Result<(), CompactError> {
        let tokens_before = compaction::context_tokens(&self.context);
        let Some(cut) = compaction::find_cut(&self.context.items, keep_tokens) else {
            return Err(CompactError::Nothing);
        };
        let split = compaction::is_split_turn(&self.context.items, cut);
        let prompt = compaction::summary_prompt(
            &self.context.items[..cut],
            self.context.summary.as_deref(),
            split,
        );
        self.emit(EngineEvent::CompactionStarted { automatic })
            .await;
        let summary = match self.summarise(&prompt).await {
            Ok(summary) => summary,
            Err(e) => {
                let message = match &e {
                    CompactError::Failed(reason) => reason.clone(),
                    CompactError::Cancelled(_) | CompactError::Nothing => "stopped".to_owned(),
                };
                self.emit(EngineEvent::CompactionFailed { automatic, message })
                    .await;
                return Err(e);
            }
        };
        let first_kept = self.context.items[cut].entry_id.clone();
        self.context = compaction::compacted(&self.context, cut, summary.clone());
        let tokens_after = self
            .context
            .messages()
            .iter()
            .map(compaction::estimate_tokens)
            .sum::<u64>();
        self.compaction_guard = Some(tokens_after);
        let id = self.history.next_id();
        let at = self.shared.clock.now();
        if let Err(e) = self.history.write(&Entry::Compaction {
            id,
            at,
            summary,
            first_kept,
            tokens_before,
            tokens_after,
        }) {
            tracing::warn!(error = %e, "couldn't record a compaction");
        }
        self.emit(EngineEvent::Compacted {
            automatic,
            tokens_before: Some(tokens_before),
            tokens_after: Some(tokens_after),
        })
        .await;
        Ok(())
    }

    /// The summary request and its checks; any error leaves the context unchanged.
    async fn summarise(&mut self, prompt: &str) -> Result<String, CompactError> {
        let summary = match self.call_model(CallMode::Summary, Some(prompt)).await {
            CallOutcome::Done(message) => {
                if !message.tool_calls().is_empty() {
                    return Err(CompactError::Failed(
                        "the summary request tried to call a tool".into(),
                    ));
                }
                if matches!(
                    &message,
                    Message::Assistant {
                        stop: StopReason::MaxTokens,
                        ..
                    }
                ) {
                    return Err(CompactError::Failed(
                        "the summary hit the output limit".into(),
                    ));
                }
                message.text()
            }
            CallOutcome::Cancelled(cancel, _) => return Err(CompactError::Cancelled(cancel)),
            CallOutcome::Failed(failure) => return Err(CompactError::Failed(failure.message)),
        };
        if summary.trim().is_empty() {
            return Err(CompactError::Failed("the summary came back empty".into()));
        }
        Ok(summary)
    }

    /// A passive context reading for the UI: the auto-compact point mirrors
    /// [`compaction::auto_compaction_due`], guard included, and changes nothing.
    pub(crate) fn context_reading(
        &self,
        tokens: Option<u64>,
        basis: ContextBasis,
    ) -> ContextReading {
        let setting = u32::from(self.shared.settings.borrow().compaction.autocompact_window);
        let threshold = compaction::threshold(self.model.context_window, setting);
        let point = compaction::trigger_point(threshold, self.compaction_guard);
        ContextReading::new(tokens, basis, false, AutocompactPoint::At { tokens: point })
            .with_window(Some(u64::from(self.model.context_window)))
            .with_model(Some(self.spec.model.to_string()))
            .with_reports(crate::adapters::reported_usage_for(
                self.spec.account.provider,
            ))
    }
}

/// Why a compaction didn't happen.
pub(crate) enum CompactError {
    /// Nothing old enough to summarise.
    Nothing,
    /// The summary request failed.
    Failed(String),
    /// A command stopped it (the turn ends; a close is already recorded).
    Cancelled(Cancel),
}

fn interrupted(_cancel: Cancel, final_text: String) -> TurnEnd {
    TurnEnd {
        outcome: TurnOutcome::Interrupted,
        final_text,
    }
}

fn provider_failure(message: String) -> TurnFailure {
    TurnFailure {
        kind: FailureKind::Provider,
        message,
    }
}

fn error_result(id: &str, name: &str, text: &str) -> Message {
    Message::ToolResult {
        call_id: id.to_owned(),
        name: name.to_owned(),
        content: vec![Block::text(text)],
        is_error: true,
    }
}

fn has_content(message: &Message) -> bool {
    matches!(message, Message::Assistant { content, .. } if !content.is_empty())
}

/// A turn's input as one user message; images are read from their files.
async fn user_message(input: &TurnInput) -> Message {
    let mut content = Vec::with_capacity(input.blocks.len());
    for block in &input.blocks {
        match block {
            InputBlock::Text { text } => content.push(Block::text(text.clone())),
            InputBlock::Image { path, media_type } => match tokio::fs::read(path).await {
                Ok(bytes) => content.push(Block::Image {
                    media_type: media_type.clone(),
                    bytes,
                }),
                Err(e) => content.push(Block::text(format!(
                    "(the image {} could not be read: {e})",
                    path.display()
                ))),
            },
        }
    }
    Message::User { content }
}
