//! The Claude engine.
//!
//! Implements `qm_engine::Engine` by driving your installed `claude` over its stream-json control
//! protocol, with the same argv the Agent SDK would send ([`argv`]) and our tools served as an
//! in-process MCP server (`sdkMcpServers: ["quartermaster"]`, [`mcp`]). Each agent is one
//! long-lived `claude` process in streaming-input mode, spawned through `qm_engine::supervisor` in
//! its own process group with an environment built from scratch ([`env`](mod@env): no
//! `ANTHROPIC_API_KEY`, no Bedrock or Vertex switches; `CLAUDE_CONFIG_DIR` is never set or
//! changed).
//!
//! The session task ([`session`]) maps the protocol onto `qm_engine::EngineEvent`: the `initialize`
//! answer plus two zero-token queries become `SessionReady` with the init report, stream events
//! become committed text lines, MCP `tools/call` becomes `ToolCall`, `result` becomes `TurnEnded`
//! with usage and cost, and the built-in `EndConversation` becomes `EndConversationCalled`. It
//! shuts an idle process down after the spec's idle timeout, resumes it by the stored session ID,
//! mirrors the transcript into the agent's `session/` folder after every turn and restores it
//! before resuming if Claude's copy is gone ([`transcript`]). [`check`] is the startup check;
//! [`detect`] finds `claude` and its login.

pub mod argv;
pub mod autocompact;
pub mod check;
pub mod detect;
pub mod env;
pub mod jobs;
pub mod mcp;
pub mod plugin;
pub mod protocol;
pub mod session;
pub mod transcript;

use crate::detect::{ClaudeInstall, LoginState};
use qm_engine::supervisor::Supervisor;
use qm_engine::{
    AccountRef, Builtins, DetectedModel, Engine, EngineError, SessionHandle, SessionSpec,
    SystemPrompt, UsageReading, session_channel,
};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::domain::{Effort, EngineKind, Provider, Role};
use qm_types::ids::ModelId;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::watch;

/// The name of our in-process MCP server; our tools reach the model as
/// `mcp__quartermaster__<name>`.
pub const SERVER_NAME: &str = "quartermaster";

/// What the Claude engine needs from the app.
pub struct ClaudeConfig {
    /// The `claude` to run: from [`detect::detect`].
    pub claude: PathBuf,
    pub supervisor: Arc<dyn Supervisor>,
    pub clock: Arc<dyn Clock>,
    /// A snapshot of the app's own environment at startup. Only the keys in
    /// [`env::COPIED_KEYS`] (and `CLAUDE_CONFIG_DIR`, unchanged) are ever passed on.
    pub inherited_env: BTreeMap<String, String>,
    /// The app's settings, as they change. Each session follows them: a changed autocompact
    /// window restarts its `claude` at the next idle moment (see [`autocompact`]).
    pub settings: watch::Receiver<Config>,
}

impl fmt::Debug for ClaudeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The inherited environment is never printed: it may hold secrets of your shell's.
        f.debug_struct("ClaudeConfig")
            .field("claude", &self.claude)
            .field("supervisor", &self.supervisor)
            .field("clock", &self.clock)
            .field("inherited_env_keys", &self.inherited_env.len())
            .finish_non_exhaustive()
    }
}

/// The Claude engine.
#[derive(Clone, Debug)]
pub struct ClaudeEngine {
    shared: Arc<Shared>,
}

/// State every session of the engine shares.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) config: ClaudeConfig,
    install: Mutex<Option<ClaudeInstall>>,
    /// `initialize.models` from the last session that started (or the probe).
    models: Mutex<Option<Vec<Value>>>,
    /// The latest usage windows seen in a `rate_limit_event`.
    usage: Mutex<Option<UsageReading>>,
    /// The latest `system/init`, accounts redacted (for the isolation-sentinel check).
    last_system_init: Mutex<Option<Value>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    pub(crate) fn set_models(&self, models: Vec<Value>) {
        *lock(&self.models) = Some(models);
    }

    pub(crate) fn set_usage(&self, reading: UsageReading) {
        *lock(&self.usage) = Some(reading);
    }

    pub(crate) fn set_system_init(&self, init: Value) {
        *lock(&self.last_system_init) = Some(init);
    }
}

impl ClaudeEngine {
    pub fn new(config: ClaudeConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                install: Mutex::new(None),
                models: Mutex::new(None),
                usage: Mutex::new(None),
                last_system_init: Mutex::new(None),
            }),
        }
    }

    pub fn config(&self) -> &ClaudeConfig {
        &self.shared.config
    }

    /// Detects the configured `claude` again (`--version`, `auth status`) and remembers the
    /// result; `open` refuses sessions unless it found a subscription login. Blocks for up to a
    /// few seconds: run it off the async threads.
    pub fn redetect(&self) -> ClaudeInstall {
        let install = detect::detect_at(
            &self.shared.config.claude,
            &self.shared.config.inherited_env,
        );
        *lock(&self.shared.install) = Some(install.clone());
        install
    }

    /// Hands the engine a detection the app already ran.
    pub fn set_install(&self, install: ClaudeInstall) {
        *lock(&self.shared.install) = Some(install);
    }

    /// The last detection, if any ran.
    pub fn install(&self) -> Option<ClaudeInstall> {
        lock(&self.shared.install).clone()
    }

    /// The latest `system/init` any session saw, with accounts redacted.
    pub fn last_system_init(&self) -> Option<Value> {
        lock(&self.shared.last_system_init).clone()
    }

    /// The cached `initialize.models`, if a session or the probe has started.
    pub fn cached_models(&self) -> Option<Vec<DetectedModel>> {
        lock(&self.shared.models).as_deref().map(detected_models)
    }

    /// Refuses specs this engine can't run, with a plain sentence.
    fn validate(spec: &SessionSpec) -> Result<(), EngineError> {
        if spec.account.provider != Provider::Claude {
            return Err(EngineError::Unsupported(format!(
                "the Claude engine runs Claude subscription accounts only, not {}",
                spec.account.provider
            )));
        }
        match (spec.role, &spec.prompt) {
            (Role::Crewmate, SystemPrompt::Replace { .. }) => {
                return Err(EngineError::Unsupported(
                    "a crewmate on the Claude engine keeps Claude Code's own prompt with ours appended"
                        .into(),
                ));
            }
            (Role::Quartermaster | Role::Secondmate, SystemPrompt::AppendToClaudeCode { .. }) => {
                return Err(EngineError::Unsupported(
                    "an orchestrator on the Claude engine gets our whole prompt, not Claude Code's"
                        .into(),
                ));
            }
            _ => {}
        }
        // The role decides the built-in tools (`--tools=` for orchestrators, Claude Code's own for
        // crewmates); only the provider engine's file tools are impossible here.
        if let Builtins::ProviderFileTools { .. } = spec.builtins {
            return Err(EngineError::Unsupported(
                "the Claude engine offers Claude Code's own tools, not the provider file tools"
                    .into(),
            ));
        }
        env::check_spec_env(&spec.env)
    }
}

impl Engine for ClaudeEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Claude
    }

    fn open(&self, spec: SessionSpec) -> Result<SessionHandle, EngineError> {
        Self::validate(&spec)?;
        // The core detects at startup and hands the result over; a developer run that
        // didn't is detected here, once.
        let install = match self.install() {
            Some(install) => install,
            None => self.redetect(),
        };
        if !matches!(install.login, LoginState::Subscription { .. }) {
            return Err(EngineError::Unavailable(
                install.unavailable_reason().unwrap_or_else(|| {
                    "the Claude engine is off: no Claude subscription login was found".into()
                }),
            ));
        }
        let (handle, side) = session_channel();
        let session = session::Session::new(Arc::clone(&self.shared), spec, side);
        tokio::spawn(session.run());
        Ok(handle)
    }

    fn models<'a>(
        &'a self,
        _account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Vec<DetectedModel>, EngineError>> {
        Box::pin(async move {
            if let Some(models) = self.cached_models() {
                return Ok(models);
            }
            let report = check::probe(self, Vec::new()).await?;
            Ok(report.models)
        })
    }

    fn usage<'a>(
        &'a self,
        _account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Option<UsageReading>, EngineError>> {
        // Never starts a process for usage: the latest `rate_limit_event` seen, if any.
        Box::pin(async move { Ok(lock(&self.shared.usage).clone()) })
    }
}

/// `initialize.models` as detected models: one per concrete model ID (`resolvedModel`, else the
/// alias), with its reported effort levels; the "default" alias only when nothing else names it.
pub fn detected_models(models: &[Value]) -> Vec<DetectedModel> {
    let mut out: Vec<DetectedModel> = Vec::new();
    let mut ordered: Vec<&Value> = models
        .iter()
        .filter(|m| m.get("value").and_then(Value::as_str) != Some("default"))
        .collect();
    ordered.extend(
        models
            .iter()
            .filter(|m| m.get("value").and_then(Value::as_str) == Some("default")),
    );
    for model in ordered {
        let id = model
            .get("resolvedModel")
            .and_then(Value::as_str)
            .or_else(|| model.get("value").and_then(Value::as_str));
        let Some(Ok(id)) = id.map(ModelId::new) else {
            continue;
        };
        if out.iter().any(|m| m.id == id) {
            continue;
        }
        let efforts = model
            .get("supportedEffortLevels")
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|l| l.parse::<Effort>().ok())
                    .collect()
            })
            .unwrap_or_default();
        out.push(DetectedModel {
            id,
            display_name: model
                .get("displayName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            efforts,
            image_input: None,
            context_window: None,
        });
    }
    out
}
