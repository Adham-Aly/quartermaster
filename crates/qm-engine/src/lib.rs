//! The engine interface both engines implement.
//!
//! An [`Engine`] opens sessions. A session is driven entirely through its [`SessionHandle`]: the
//! core sends [`SessionCommand`]s and reads [`EngineEvent`]s. Both are plain serde data, so the
//! scripted engine and a future socket all speak the same language.
//!
//! One session's life, as the core sees it:
//!
//! 1. `Engine::open(spec)` starts a new session, or resumes one by its stored
//!    [`SessionRef`](qm_types::ids::SessionRef) (`spec.resume`). The engine answers with
//!    [`EngineEvent::SessionReady`], carrying the reference to store on the agent and the engine's
//!    init report.
//! 2. [`SessionCommand::StartTurn`] runs one turn. The engine reports
//!    [`EngineEvent::ModelCallStarted`] / [`EngineEvent::ModelCallFinished`] around every model
//!    call, assistant text as committed [`EngineEvent::TextLine`]s, and each call to one of *our*
//!    tools as [`EngineEvent::ToolCall`]. The core runs the tool, applying every rule, and answers
//!    with [`SessionCommand::ToolResult`]; the turn continues.
//! 3. [`SessionCommand::Inject`] slips a message into the running turn: a background job finishing
//!    or your direct message to a crewmate. [`SessionCommand::Interrupt`] stops the turn and keeps
//!    the session. Long silent work shows as [`EngineEvent::Activity`], at most every
//!    [`ACTIVITY_EVERY`].
//! 4. [`EngineEvent::TurnEnded`] closes every turn, with its outcome, usage and cost, and the
//!    model's plain final text (the hidden close-out).
//! 5. A crewmate's background jobs are reported with [`EngineEvent::JobStarted`] and
//!    [`EngineEvent::JobFinished`], which may come between turns.
//! 6. Between turns, with no job running, the engine may release its process
//!    ([`EngineEvent::Suspended`]). The next turn brings it back: a new
//!    [`EngineEvent::SessionReady`] with `resumed: true` comes before its `TurnStarted`, or, if the
//!    session can't be resumed, `SessionClosed { ResumeFailed }` comes instead and no turn starts.
//!    [`SessionCommand::Close`] ends the session; [`EngineEvent::SessionClosed`] is always the
//!    last event.
//!
//! The core never names a concrete engine: the binary injects the Claude engine
//! (`qm-engine-claude`), the provider engine (`qm-providers`) and the scripted engine
//! ([`scripted`]). Limits are the core's: it feeds the events into a [`TurnMeter`] and checks the
//! resulting [`TurnCounters`] against `qm-policy`; the two an engine must apply inside its own
//! request loop travel in [`SessionSpec::limits`]. Processes are started and stopped only through
//! the [`supervisor`].

mod activity;
mod engine;
mod event;
mod lines;
mod meter;
pub mod scripted;
mod session;
mod spec;
pub mod supervisor;
mod tool;

pub use activity::{ACTIVITY_EVERY, ActivityGate};
pub use engine::{DetectedModel, Engine, EngineError, UsageReading, UsageWindow};
pub use event::{
    ActivityKind, CallUsage, CloseReason, EngineEvent, FailureKind, InitReport, ProviderError,
    ProviderErrorKind, RateLimitInfo, RateLimitStatus, StopReason, TurnFailure, TurnOutcome,
    TurnUsage,
};
pub use lines::LineBuffer;
pub use meter::{TurnCounters, TurnMeter};
pub use session::{
    EVENT_BUFFER, EngineSide, InputBlock, SessionCommand, SessionHandle, TurnInput, session_channel,
};
pub use spec::{AccountRef, Builtins, SessionLimits, SessionSpec, SystemPrompt};
pub use tool::{ToolCallId, ToolDescriptor, ToolResult};
