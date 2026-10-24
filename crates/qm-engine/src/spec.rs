//! What it takes to open a session.

use crate::tool::ToolDescriptor;
use qm_types::domain::{Effort, Provider, Role};
use qm_types::ids::{AccountId, AgentId, ModelId, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Everything an engine needs to start or resume one agent's session. The core builds it from the
/// agent's record, the routing decision and the rendered prompt; the engine adds nothing of its own
/// from your machine's setup.
///
/// `#[non_exhaustive]`, so a field added later (with a default) breaks no other crate: build it
/// with [`SessionSpec::new`] and set the optional fields after.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionSpec {
    pub agent: AgentId,
    pub role: Role,
    /// The account that serves the model; it decides the engine.
    pub account: AccountRef,
    pub model: ModelId,
    /// Fixed for the conversation: a resumed session keeps the level it started with, so the core
    /// passes the stored level back.
    pub effort: Effort,
    /// The agent's working folder: its `home/`, or the task's worktree.
    pub cwd: PathBuf,
    /// The agent's `session/` folder: the engine's working state for the process. The Claude
    /// engine mirrors its transcript here after every turn and restores it before resuming; the
    /// provider engine keeps background job output beside it.
    pub session_dir: PathBuf,
    /// Resume this session instead of starting a new one.
    #[serde(default)]
    pub resume: Option<SessionRef>,
    pub prompt: SystemPrompt,
    /// Our tools for this role, from the `qm-tools` registry.
    #[serde(default)]
    pub tools: Vec<ToolDescriptor>,
    /// The engine's own built-in tools, if any.
    #[serde(default)]
    pub builtins: Builtins,
    /// The skills this crewmate gets, gathered by the app into `tasks/T-0042/skills/`. The Claude
    /// engine hands them over as a local plugin folder with skills only; the provider engine's
    /// prompt already lists them.
    #[serde(default)]
    pub skills_dir: Option<PathBuf>,
    /// Variables added to the environment the engine builds from scratch for the agent's process
    /// (`QM_EVIDENCE_DIR`, `QM_REPORT_DIR`, `QM_PLAN_DIR`, `QM_PORT_BASE`,
    /// `GIT_TERMINAL_PROMPT`). Never a secret, and never
    /// one of `qm_types::env::NEVER_PASSED_TO_AGENTS`.
    ///
    /// They are added to what the engine sets itself and may not replace it: a spec naming a key
    /// the engine owns (`HOME`, `PATH`, `TMPDIR`, the engine's own switches), or one of
    /// `NEVER_PASSED_TO_AGENTS`, is refused by `Engine::open` with `EngineError::Unsupported`, so
    /// the spec can never undo the isolation the engine builds, and a collision is loud rather than
    /// silently resolved. Each engine documents the keys it owns.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Release the process after this long idle and resume on the next turn (5 minutes for
    /// waiting Claude agents). Idle means no turn and no background job running.
    /// `None` keeps it up, as for the Quartermaster.
    #[serde(default)]
    pub idle_shutdown: Option<Duration>,
    /// Limits the engine applies itself: the orchestrators', which the core fills from `qm-policy`.
    /// `None` for crewmates, whose turns have no limits of ours.
    #[serde(default)]
    pub limits: Option<SessionLimits>,
    /// The provider engine keeps this session's current transcript here (`<session>.jsonl`), so it
    /// can be resumed after the app stops; `None` keeps it in memory only (helpers). The Claude
    /// engine ignores it: `claude` keeps its own transcripts.
    #[serde(default)]
    pub transcript_dir: Option<PathBuf>,
    /// Folders the provider engine's file tools never write into except through a write root
    /// inside them: the app's data directory and scratch root. Empty protects only the default
    /// scratch base.
    #[serde(default)]
    pub protected_roots: Vec<PathBuf>,
}

impl SessionSpec {
    /// A spec with the required parts. Everything else starts empty: no resume, no tools, no
    /// built-in tools, no skills, nothing added to the environment, no idle shutdown, no limits.
    #[allow(clippy::too_many_arguments)] // Each one is required, and naming them beats a builder.
    pub fn new(
        agent: AgentId,
        role: Role,
        account: AccountRef,
        model: ModelId,
        effort: Effort,
        cwd: PathBuf,
        session_dir: PathBuf,
        prompt: SystemPrompt,
    ) -> Self {
        Self {
            agent,
            role,
            account,
            model,
            effort,
            cwd,
            session_dir,
            resume: None,
            prompt,
            tools: Vec::new(),
            builtins: Builtins::None,
            skills_dir: None,
            env: BTreeMap::new(),
            idle_shutdown: None,
            limits: None,
            transcript_dir: None,
            protected_roots: Vec::new(),
        }
    }

    /// Keeps the provider engine's transcript of this session in `dir`.
    #[must_use]
    pub fn with_transcript_dir(mut self, dir: PathBuf) -> Self {
        self.transcript_dir = Some(dir);
        self
    }

    /// Protects `roots` from the provider engine's file tools (see [`Self::protected_roots`]).
    #[must_use]
    pub fn with_protected_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.protected_roots = roots;
        self
    }
}

/// Limits an engine applies inside its own request loop, because only it sees each request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLimits {
    /// The most output tokens one model message may produce, thinking included (8,192 for the
    /// orchestrators). The provider engine sends it as the request's output limit; the
    /// Claude engine passes it to `claude`. Reaching it ends the call with `StopReason::MaxTokens`,
    /// and the turn's failure names the cap.
    pub max_output_tokens: u32,
    /// Retries per model call before the turn fails with `FailureKind::Provider` (2).
    pub max_retries: u32,
}

/// The account serving a session, as far as an engine needs to know it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRef {
    pub id: AccountId,
    pub provider: Provider,
    /// The key of its credentials in the credential store; never the secret. `None` for a Claude
    /// subscription, whose login `claude` keeps itself.
    pub credential: Option<String>,
    /// The base URL of a custom OpenAI-compatible endpoint.
    pub base_url: Option<String>,
}

/// The system prompt, which is always ours.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SystemPrompt {
    /// The whole prompt: orchestrators on both engines, and crewmates on the provider engine.
    Replace { text: String },
    /// Claude Code's own `claude_code` preset, with this appended: crewmates on the Claude engine,
    /// which must name the preset explicitly (leaving it out gives an empty prompt). Other engines
    /// refuse it with `EngineError::Unsupported`.
    AppendToClaudeCode { text: String },
}

/// The engine's own built-in tools.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "builtins", rename_all = "snake_case")]
pub enum Builtins {
    /// None: the Quartermaster and secondmates (`--tools=` on Claude).
    #[default]
    None,
    /// Claude Code's built-in tools, permission prompts off: crewmates on the Claude engine.
    ClaudeCode,
    /// Our file and shell tools: crewmates on the provider engine. Writes are refused outside these
    /// folders (the worktree, evidence, report, plan for a planner, and scratchpad)
    /// and the system temporary folder, and inside the spec's `protected_roots` only these folders
    /// are writable. `scratchpad`, when
    /// set, is the agent's private scratch folder: it is also a write root, and the engine tells
    /// the model to use it for temporary files.
    ProviderFileTools {
        write_roots: Vec<PathBuf>,
        #[serde(default)]
        scratchpad: Option<PathBuf>,
    },
}
