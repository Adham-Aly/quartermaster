//! The child's environment, built from scratch.
//!
//! Nothing is inherited except the few keys copied from the app's own environment snapshot, so a
//! stray `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or Bedrock/Vertex switch in your shell can
//! never move an agent onto per-token billing. `CLAUDE_CONFIG_DIR` is passed through unchanged
//! only when the app itself was started with it, and is never set or changed: changing it logs a
//! subscription user out on macOS.
//!
//! Claude Code's cross-session messaging is switched off (`CLAUDE_CODE_HARBOR_KITE=0`): with it
//! on, every `claude` listens on a socket in a shared folder (`/tmp/cc-socks`), so
//! any other Claude Code session on the Mac could message an agent, and an agent could message
//! them. With it off no socket is opened. The tools that send are off as well
//! (the harness tools in [`crate::argv`]).
//!
//! The engine owns every key in [`OWNED_KEYS`]: a `spec.env` naming one of them, one of
//! `qm_types::env::NEVER_PASSED_TO_AGENTS`, or `CLAUDE_CONFIG_DIR` is refused with
//! `EngineError::Unsupported`, so a spec can never undo the isolation.

use qm_engine::{EngineError, SessionSpec};
use qm_types::domain::Role;
use qm_types::env::NEVER_PASSED_TO_AGENTS;
use std::collections::BTreeMap;

/// The Agent SDK version whose wire format the engine mirrors.
pub const MIRRORED_SDK_VERSION: &str = "0.3.302";

/// Keys copied from the app's environment when present there.
pub const COPIED_KEYS: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];

/// Passed through unchanged only if the app's own environment has it; never set or changed.
pub const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

/// Every key the engine sets or copies itself. A `spec.env` naming one is refused.
pub const OWNED_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    CLAUDE_CONFIG_DIR,
    "ENABLE_TOOL_SEARCH",
    "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
    "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_CODE_SDK_READS_SESSION_STATE",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_MAX_RETRIES",
    CROSS_SESSION_MESSAGING,
];

/// Claude Code's switch for cross-session messaging (its `isCrossSessionMessagingEnabled`), set
/// to `0` for every agent.
pub const CROSS_SESSION_MESSAGING: &str = "CLAUDE_CODE_HARBOR_KITE";

/// The fixed switches every agent gets (what the Agent SDK adds, plus ours: no cross-session
/// messaging).
const SWITCHES: &[(&str, &str)] = &[
    ("ENABLE_TOOL_SEARCH", "false"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
    ("CLAUDE_AGENT_SDK_VERSION", MIRRORED_SDK_VERSION),
    ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1"),
    (CROSS_SESSION_MESSAGING, "0"),
];

/// The environment without any session's additions: the copied keys, `CLAUDE_CONFIG_DIR` when
/// the app has it, and the fixed switches. `detect` runs `claude --version` and `claude auth
/// status` with it, so it sees the same login an agent will.
pub fn base_env(inherited: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in COPIED_KEYS {
        if let Some(value) = inherited.get(*key) {
            env.insert((*key).to_owned(), value.clone());
        }
    }
    if let Some(value) = inherited.get(CLAUDE_CONFIG_DIR) {
        env.insert(CLAUDE_CONFIG_DIR.to_owned(), value.clone());
    }
    for (key, value) in SWITCHES {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    env
}

/// The complete environment for one session's `claude`: [`base_env`], the orchestrators' output
/// cap and retries from `spec.limits`, then `spec.env` on top. Refuses a `spec.env` naming an
/// owned or forbidden key.
pub fn build_env(
    inherited: &BTreeMap<String, String>,
    spec: &SessionSpec,
) -> Result<BTreeMap<String, String>, EngineError> {
    check_spec_env(&spec.env)?;
    let mut env = base_env(inherited);
    if let (Some(limits), Role::Quartermaster | Role::Secondmate) = (spec.limits, spec.role) {
        env.insert(
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_owned(),
            limits.max_output_tokens.to_string(),
        );
        env.insert(
            "CLAUDE_CODE_MAX_RETRIES".to_owned(),
            limits.max_retries.to_string(),
        );
    }
    for (key, value) in &spec.env {
        env.insert(key.clone(), value.clone());
    }
    // Belt and braces: nothing above copies these, and nothing ever may.
    for key in NEVER_PASSED_TO_AGENTS {
        env.remove(*key);
    }
    Ok(env)
}

/// Refuses a `spec.env` that names a key the engine owns or one that never reaches an agent.
pub fn check_spec_env(spec_env: &BTreeMap<String, String>) -> Result<(), EngineError> {
    for key in spec_env.keys() {
        if NEVER_PASSED_TO_AGENTS.contains(&key.as_str()) {
            return Err(EngineError::Unsupported(format!(
                "{key} is never passed to an agent, so a session can't set it"
            )));
        }
        if OWNED_KEYS.contains(&key.as_str()) {
            return Err(EngineError::Unsupported(format!(
                "the Claude engine sets {key} itself, so a session can't set it"
            )));
        }
    }
    Ok(())
}
