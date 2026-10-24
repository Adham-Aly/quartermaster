//! The event stream: everything an engine reports about a session.

use crate::context::ContextReading;
use crate::tool::ToolCallId;
use jiff::Timestamp;
use qm_types::ids::{JobLabel, ModelId, SessionRef};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One thing that happened in a session, in order. Every engine keeps this order:
///
/// 1. `SessionReady` comes first (or `SessionClosed`, if the session couldn't start). Nothing
///    follows `SessionClosed`, and before it every turn has ended and every job has finished.
/// 2. Turns don't overlap. Model calls, text, tool calls, built-in tool use, `Activity`, retries,
///    `JobStarted` and `EndConversationCalled` happen only inside a turn. Calls count from 1 in
///    each turn, one at a time, and `ModelCallFinished` names the call in progress.
/// 3. A job label starts once and finishes once; `JobFinished` may come between turns.
/// 4. After `Suspended` (only between turns, with no job running), or a turn that failed because
///    the process exited, the process is down. The next turn brings it back with
///    `SessionReady { resumed: true }` and then `TurnStarted`; if it can't, the engine sends
///    `SessionClosed` with `ResumeFailed` and no `TurnStarted`, and the core keeps that input.
/// 5. `SessionReady` comes only first, or to end a spell with the process down.
/// 6. After `EndConversationCalled` the turn ends and the session closes; no new turn starts.
/// 7. `SessionCommand::Compact` runs as a turn of its own: `TurnStarted`, then `Compacted
///    { automatic: false }` if the conversation was compacted, then `TurnEnded` (`Completed`
///    when it ran, even with nothing to compact; `Failed` with the reason when it couldn't).
///    No text, tool calls or jobs come in it.
/// 8. `ContextObserved` may come inside a turn (right after a `ModelCallFinished`) or between
///    turns (after `SessionReady`, or after a model change). `CompactionStarted` comes before the
///    `Compacted` or `CompactionFailed` it starts, inside a turn, inside `Compact`'s own turn, or
///    between turns. None of the three comes after `SessionClosed`, and no engine has to send
///    them: a consumer treats their absence as "unknown". They are passive and change nothing.
///
/// `#[non_exhaustive]`: engines may gain events without breaking the core's `match`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EngineEvent {
    /// The session's process is up: store `session` on the agent to resume it later (every time: a
    /// resume may hand back a new reference). `init` is what the engine reported about itself; the
    /// core checks its tools and permission mode against ours each time. It comes first, and again
    /// whenever a turn brings back a session whose process had gone (after `Suspended`, or after a
    /// turn that failed because the process exited), then with `resumed: true` and before
    /// `TurnStarted`.
    SessionReady {
        session: SessionRef,
        resumed: bool,
        init: InitReport,
    },
    /// A turn began: after `StartTurn`, or with none when the engine adopts a turn its agent
    /// started by itself (Claude Code does, to tell the model a background task ended). Either
    /// way the usual turn events and `TurnEnded` follow, and turns never overlap.
    TurnStarted,
    /// A request to the model began; `call` counts from 1 within the turn (the 40-calls limit).
    ModelCallStarted { call: u32 },
    /// One complete line of assistant text, committed so nothing reflows. The partial last line of
    /// a text block is flushed when the block ends.
    TextLine { text: String },
    /// The model called one of our tools. The core answers with `SessionCommand::ToolResult`
    /// carrying the same `call`; the per-call clock pauses until then.
    ToolCall {
        call: ToolCallId,
        name: String,
        args: serde_json::Value,
    },
    /// The engine ran one of its own built-in tools (Claude Code's on Claude, the file tools on the
    /// provider engine): for the worker session view ("▸ Bash cargo test auth") and as a sign of
    /// life.
    BuiltinToolUse { name: String, summary: String },
    /// Still working with nothing to commit: the model is thinking (or writing a long tool call),
    /// or a built-in tool is still running. Sent at most once per [`crate::ACTIVITY_EVERY`] of
    /// otherwise silent work ([`crate::ActivityGate`]), and only inside a turn, so the mid-turn
    /// quiet watchdog can tell silence from work.
    Activity { kind: ActivityKind },
    /// A model request finished, with its usage. `usage.output_tokens` includes thinking: the
    /// orchestrators' 8,192-token cap is per message, thinking included.
    ModelCallFinished {
        call: u32,
        usage: CallUsage,
        stop: StopReason,
    },
    /// A model request failed and will be retried (2 retries per call).
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: ProviderError,
        delay_ms: u64,
    },
    /// A rate-limit report for the account. `Rejected` means the account hit its limit: its agents
    /// wait for the reset, which counts as a sign of life.
    RateLimit { info: RateLimitInfo },
    /// The engine compacted the conversation: by itself (`automatic`), or because the core sent
    /// `SessionCommand::Compact`. The chat shows it.
    Compacted {
        automatic: bool,
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
    },
    /// A passive reading of how full the context is. Never changes what the session does.
    ContextObserved { reading: ContextReading },
    /// The engine began compacting (by itself, or because the core sent `Compact`).
    CompactionStarted { automatic: bool },
    /// A compaction that started didn't complete; the context is unchanged.
    CompactionFailed { automatic: bool, message: String },
    /// A background shell job started, inside a turn: `J3` on the provider engine, whose supervisor
    /// starts it; Claude Code's own job name on Claude, which starts it itself. The core writes the
    /// `jobs` row and counts a running job as a sign of life for up to two hours. The engine keeps
    /// the session's process up while any job runs.
    JobStarted {
        job: JobLabel,
        /// The command as the crewmate wrote it.
        command: String,
        /// Where the job's output is written, when the engine knows (the provider engine writes
        /// `tasks/C-0042/jobs/J3.log`).
        log_path: Option<PathBuf>,
        /// How long the command had already run when it became a job, when the engine knows:
        /// Claude reports a `Bash` call it moved to the background only then. `None` means it
        /// starts now.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ran_for_ms: Option<u64>,
    },
    /// A background job finished, for any reason; between turns too. Every `JobStarted` gets
    /// exactly one, before `SessionClosed` and before the process goes down: a job that dies with
    /// its session is reported with `exit_code: None`. Unless `agent_told`, the core wakes the
    /// crewmate with it (into its running turn, or a new one once it is idle).
    JobFinished {
        job: JobLabel,
        exit_code: Option<i32>,
        tail: String,
        /// The agent's own harness has already shown the model this job's end (Claude Code's
        /// task notification), so the core posts no wake for it.
        #[serde(default)]
        agent_told: bool,
    },
    /// The model called Claude's built-in `EndConversation`, which can't be removed. The turn then
    /// ends (`TurnEnded`) and the session closes (`SessionClosed { EndConversation }`); the core
    /// records it and opens a new session.
    EndConversationCalled,
    /// The turn is over. `final_text` is the model's plain final text: the hidden close-out for
    /// orchestrators, and the stored report for a crewmate that didn't call `report`.
    TurnEnded {
        outcome: TurnOutcome,
        usage: TurnUsage,
        final_text: String,
    },
    /// The engine released this session's process after `spec.idle_shutdown` with no turn and no
    /// job running. The next `StartTurn` (or `Inject`) brings it back: the engine reports
    /// `SessionReady { resumed: true }` and then `TurnStarted`, or, if it can't resume,
    /// `SessionClosed { ResumeFailed }` with no `TurnStarted`: that turn's input was never
    /// delivered, so the core keeps it for a new session.
    Suspended,
    /// The session is over; no event follows.
    SessionClosed { reason: CloseReason },
}

impl EngineEvent {
    /// The event's wire name (its `event` tag), for logs and error messages.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SessionReady { .. } => "session_ready",
            Self::TurnStarted => "turn_started",
            Self::ModelCallStarted { .. } => "model_call_started",
            Self::TextLine { .. } => "text_line",
            Self::ToolCall { .. } => "tool_call",
            Self::BuiltinToolUse { .. } => "builtin_tool_use",
            Self::Activity { .. } => "activity",
            Self::ModelCallFinished { .. } => "model_call_finished",
            Self::Retrying { .. } => "retrying",
            Self::RateLimit { .. } => "rate_limit",
            Self::Compacted { .. } => "compacted",
            Self::ContextObserved { .. } => "context_observed",
            Self::CompactionStarted { .. } => "compaction_started",
            Self::CompactionFailed { .. } => "compaction_failed",
            Self::JobStarted { .. } => "job_started",
            Self::JobFinished { .. } => "job_finished",
            Self::EndConversationCalled => "end_conversation_called",
            Self::TurnEnded { .. } => "turn_ended",
            Self::Suspended => "suspended",
            Self::SessionClosed { .. } => "session_closed",
        }
    }
}

/// What an engine is busy with when it reports [`EngineEvent::Activity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// The model is still generating: thinking, or writing a long tool call.
    Thinking,
    /// One of the engine's built-in tools is still running.
    BuiltinTool,
}

/// What the engine reported about itself when the session started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InitReport {
    /// e.g. `claude`'s version, checked against the supported range at startup.
    pub engine_version: Option<String>,
    pub model: Option<ModelId>,
    /// Every tool the session will offer the model, as the engine names them.
    pub tools: Vec<String>,
    /// e.g. `dontAsk` on Claude.
    pub permission_mode: Option<String>,
    /// Protocol capabilities the engine advertised.
    pub capabilities: Vec<String>,
    /// The engine's whole init payload, with account details redacted.
    pub raw: serde_json::Value,
}

/// Tokens for one model request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallUsage {
    pub input_tokens: u64,
    /// Output including thinking.
    pub output_tokens: u64,
    /// The thinking part of `output_tokens`, where reported.
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Why a model request stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    /// The output limit was reached.
    MaxTokens,
    Refusal,
    Unknown,
}

/// A turn's totals, recorded per agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnUsage {
    pub model_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Estimated cost where the engine reports it (Claude does).
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    /// Stopped by `SessionCommand::Interrupt`; the session is kept.
    Interrupted,
    Failed {
        failure: TurnFailure,
    },
}

/// Why a turn failed, in a plain sentence the agent's supervisor and you can read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFailure {
    pub kind: FailureKind,
    pub message: String,
}

/// The broad reason a turn failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The provider kept failing after its retries.
    Provider,
    /// The account hit its usage limit.
    AccountLimit,
    /// The engine's process exited mid-turn (a crash). The session is kept with its process down,
    /// as after `Suspended`: the next turn brings it back with `SessionReady { resumed: true }`.
    ProcessExited,
    /// The engine and the app disagreed about the protocol.
    Protocol,
    Internal,
}

/// An error from the model provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    /// The HTTP status, where there was one.
    pub status: Option<u16>,
    pub message: String,
}

/// The broad kind of a provider error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    RateLimited,
    Overloaded,
    Server,
    Network,
    Auth,
    InvalidRequest,
    ContextOverflow,
    Other,
}

/// A rate-limit report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    pub status: RateLimitStatus,
    /// Which window, e.g. `five_hour`, `seven_day`.
    pub window: Option<String>,
    pub resets_at: Option<Timestamp>,
}

/// Where an account stands against its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
}

/// Why a session closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum CloseReason {
    /// After `SessionCommand::Close`.
    Requested,
    /// After `EndConversationCalled`.
    EndConversation,
    /// The stored session couldn't be resumed.
    ResumeFailed { message: String },
    /// The engine's process died and the session can't continue.
    Crashed {
        exit_code: Option<i32>,
        signal: Option<i32>,
        stderr_tail: String,
    },
}
