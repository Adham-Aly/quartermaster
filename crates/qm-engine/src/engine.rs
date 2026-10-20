//! The `Engine` trait.

use crate::event::ProviderError;
use crate::session::SessionHandle;
use crate::spec::{AccountRef, SessionSpec};
use jiff::Timestamp;
use qm_types::BoxFuture;
use qm_types::domain::{Effort, EngineKind};
use qm_types::ids::ModelId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// An engine: the Claude engine, the provider engine, or the scripted engine.
///
/// Object safe: the core holds engines as `Arc<dyn Engine>`, keyed by [`EngineKind`].
///
/// Every engine:
/// - builds each agent's environment from scratch (only the spec's `env` on top of what it needs,
///   refusing a spec whose `env` names a key it owns or one of
///   `qm_types::env::NEVER_PASSED_TO_AGENTS`) and loads no setup of its own: no hooks, settings,
///   MCP servers, memory or instruction files beyond what the spec hands it;
/// - starts every process through `crate::supervisor`;
/// - commits text a line at a time with [`crate::LineBuffer`], and reports silent work (long
///   thinking, a long built-in tool) as `Activity` through [`crate::ActivityGate`];
/// - reports `ModelCallStarted`/`ModelCallFinished` around every model request, so the core's
///   limits work the same on every engine, and applies `spec.limits` itself;
/// - reports every background job with `JobStarted` and exactly one `JobFinished`, and **never
///   releases the process while a job runs**: idle means no turn *and* no job;
/// - after `spec.idle_shutdown` idle, releases the session's process and reports `Suspended`; the
///   next turn brings it back with `SessionReady { resumed: true }` before `TurnStarted`, or
///   reports `SessionClosed { ResumeFailed }` and starts no turn;
/// - keeps the transcript in `spec.session_dir`: the provider engine writes its JSONL there; the
///   Claude engine mirrors Claude's transcript there after every turn and restores it before
///   resuming if Claude's copy is gone;
/// - ends every session with exactly one `SessionClosed`.
pub trait Engine: Send + Sync + fmt::Debug {
    fn kind(&self) -> EngineKind;

    /// Starts a new session, or resumes `spec.resume`. Returns at once; the engine reports
    /// `SessionReady` (or `SessionClosed` if it couldn't start: `ResumeFailed` for a resume) on the
    /// handle. Errors here are for specs the engine can't run at all. Needs a Tokio runtime.
    fn open(&self, spec: SessionSpec) -> Result<SessionHandle, EngineError>;

    /// The models this account offers, with each model's effort levels where known. Never blocks
    /// startup: the core calls it in the background.
    fn models<'a>(
        &'a self,
        account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Vec<DetectedModel>, EngineError>>;

    /// The account's usage windows where the provider reports them; `None` means unknown, which is
    /// never read as "all left".
    fn usage<'a>(
        &'a self,
        account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Option<UsageReading>, EngineError>>;
}

/// A model an account offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedModel {
    pub id: ModelId,
    pub display_name: Option<String>,
    /// The effort levels it supports; settings offer only these.
    pub efforts: Vec<Effort>,
    /// `None` when unknown: the catalog decides, or cautious defaults apply.
    pub image_input: Option<bool>,
    pub context_window: Option<u32>,
}

/// An account's usage, read at most every few minutes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReading {
    pub windows: Vec<UsageWindow>,
    pub read_at: Timestamp,
}

/// One usage window, e.g. Claude's 5-hour or 7-day window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// e.g. `five_hour`, `seven_day`, `seven_day_opus`.
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<Timestamp>,
}

/// What can go wrong talking to an engine. Messages are plain sentences and never carry a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The engine can't run on this Mac right now, e.g. `claude` is missing or not logged in with a
    /// subscription.
    #[error("{0}")]
    Unavailable(String),
    /// The spec asks for something this engine can't do.
    #[error("{0}")]
    Unsupported(String),
    /// The engine's process couldn't be started.
    #[error("couldn't start the engine: {0}")]
    Spawn(String),
    /// The engine and the app disagreed about the protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The provider refused or failed.
    #[error("provider error: {}", .0.message)]
    Provider(ProviderError),
    /// The session is closed.
    #[error("the session is closed")]
    Closed,
}
