//! The startup check.
//!
//! `claude`'s protocol is undocumented, so every session's init report is checked against what we
//! expect before the agent is trusted: the version is in the supported range, the protocol
//! capabilities we rely on are there, the permission mode is ours, the tools are exactly ours
//! (orchestrators) or all of ours plus only the built-ins a crewmate is granted
//! ([`crewmate_reported_builtins`]), and no MCP server but ours is connected. Each
//! refusal is one plain sentence naming what was found; the core switches the Claude engine off
//! with it.
//!
//! [`probe`] runs the check at startup without spending tokens: it opens an orchestrator and a
//! crewmate session in a throwaway folder, checks both, records the crewmate's reported tool list,
//! caches the `initialize` model list for `Engine::models`, and closes them. When `claude` defers
//! its `system/init` to the first turn (below), that list is only our MCP tools: the built-ins
//! appear in the first `system/init`, so the built-ins offered up front (`WebFetch`, `WebSearch`,
//! `NotebookEdit` and the rest) are only visible once a turn has run.
//!
//! claude 2.1.303 advertises its protocol capabilities only in `system/init`, which comes with
//! the first turn; its `initialize` answer lists a few others. When the init report's raw payload
//! says `capabilities_deferred`, the engine checks the required capabilities at the first
//! `system/init` (failing that turn with `FailureKind::Protocol`), and this check leaves them out.

use crate::argv::{ToolSet, crewmate_reported_builtins};
use crate::detect::{ClaudeInstall, version_in_range};
use crate::mcp::McpServer;
use crate::{ClaudeEngine, SERVER_NAME};
use qm_engine::{
    AccountRef, Builtins, DetectedModel, Engine, EngineError, EngineEvent, InitReport, SessionSpec,
    SystemPrompt, ToolDescriptor,
};
use qm_types::domain::{Effort, Provider, Role};
use qm_types::ids::{AccountId, AgentId, ModelId};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// The supported `claude` versions: minimum inclusive, maximum exclusive.
pub const SUPPORTED_VERSIONS: (&str, &str) = ("2.1.303", "3.0.0");

/// Protocol capabilities the engine relies on.
pub const REQUIRED_CAPABILITIES: &[&str] = &["interrupt_receipt_v1"];

/// The model the probe's zero-token sessions name.
pub const PROBE_MODEL: &str = "claude-haiku-5-5";

/// How long the probe waits for each session to become ready.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// What a session's init report must show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedSession {
    pub role: Role,
    /// Our tools as `claude` names them: `mcp__quartermaster__<name>`.
    pub tools: Vec<String>,
    pub permission_mode: &'static str,
}

impl ExpectedSession {
    pub fn new(role: Role, tools: &[ToolDescriptor]) -> Self {
        Self {
            role,
            tools: McpServer::new(tools.to_vec()).qualified_names(),
            permission_mode: ToolSet::for_role(role).permission_mode(),
        }
    }

    pub fn for_spec(spec: &SessionSpec) -> Self {
        Self::new(spec.role, &spec.tools)
    }

    fn orchestrator(&self) -> bool {
        matches!(self.role, Role::Quartermaster | Role::Secondmate)
    }
}

/// Why a session was refused, in one plain sentence.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CheckFailure(pub String);

/// Checks one init report.
pub fn check(report: &InitReport, expected: &ExpectedSession) -> Result<(), CheckFailure> {
    let (min, max) = SUPPORTED_VERSIONS;
    match &report.engine_version {
        None => {
            return Err(CheckFailure(
                "Claude Code didn't report its version, so the Claude engine is off.".into(),
            ));
        }
        Some(version) if !version_in_range(version, SUPPORTED_VERSIONS) => {
            return Err(CheckFailure(format!(
                "Claude Code {version} isn't supported (this build supports {min} up to, not \
                 including, {max}), so the Claude engine is off."
            )));
        }
        Some(_) => {}
    }

    let deferred = report
        .raw
        .get("capabilities_deferred")
        .and_then(Value::as_bool)
        == Some(true);
    if !deferred {
        for required in REQUIRED_CAPABILITIES {
            if !report.capabilities.iter().any(|c| c == required) {
                return Err(CheckFailure(format!(
                    "Claude Code doesn't support the {required} protocol capability, so the \
                     Claude engine is off."
                )));
            }
        }
    }

    match report.permission_mode.as_deref() {
        Some(mode) if mode == expected.permission_mode => {}
        Some(mode) => {
            return Err(CheckFailure(format!(
                "Claude Code started in permission mode {mode} instead of {}, so the Claude \
                 engine is off.",
                expected.permission_mode
            )));
        }
        None => {
            return Err(CheckFailure(format!(
                "Claude Code didn't report its permission mode (expected {}), so the Claude \
                 engine is off.",
                expected.permission_mode
            )));
        }
    }

    check_tools(&report.tools, expected)
        .map_err(|problem| CheckFailure(format!("{problem}, so the Claude engine is off.")))?;

    let servers = mcp_servers(&report.raw);
    if let Some(foreign) = servers.iter().find(|s| *s != SERVER_NAME) {
        return Err(CheckFailure(format!(
            "Claude Code connected the MCP server {foreign}, which isn't ours, so the Claude \
             engine is off."
        )));
    }
    Ok(())
}

/// The tool rule alone: orchestrators get exactly our tools, crewmates all of ours plus only the
/// built-ins they are granted ([`crewmate_reported_builtins`]); either may also have
/// `EndConversation`, which can't be removed. Anything else, a foreign MCP server's tool or a
/// harness tool that reaches outside the task, is refused. The error is a sentence fragment naming
/// what was found.
pub fn check_tools(tools: &[String], expected: &ExpectedSession) -> Result<(), String> {
    let ours = format!("mcp__{SERVER_NAME}__");
    if let Some(missing) = expected.tools.iter().find(|t| !tools.contains(t)) {
        return Err(format!("Claude Code didn't offer our tool {missing}"));
    }
    if let Some(foreign) = tools
        .iter()
        .find(|t| t.starts_with("mcp__") && !t.starts_with(&ours))
    {
        return Err(format!(
            "Claude Code offered {foreign}, a tool of an MCP server that isn't ours"
        ));
    }
    let granted: Vec<&str> = if expected.orchestrator() {
        Vec::new()
    } else {
        crewmate_reported_builtins()
    };
    let extra: Vec<&str> = tools
        .iter()
        .map(String::as_str)
        .filter(|t| {
            !expected.tools.iter().any(|ours| ours == t)
                && !granted.contains(t)
                && *t != "EndConversation"
        })
        .collect();
    if !extra.is_empty() {
        let role = if expected.orchestrator() {
            "an orchestrator"
        } else {
            "a crewmate"
        };
        return Err(format!(
            "Claude Code offered tools {role} must not have: {}",
            extra.join(", ")
        ));
    }
    Ok(())
}

/// Every MCP server named in the raw init payload: `mcp_servers[].name` (from `system/init`) and
/// `context_usage.mcpTools[].serverName`.
fn mcp_servers(raw: &Value) -> Vec<String> {
    let mut servers = Vec::new();
    let mut push = |name: Option<&str>| {
        if let Some(name) = name
            && !servers.iter().any(|s| s == name)
        {
            servers.push(name.to_owned());
        }
    };
    for source in [raw.get("system_init"), Some(raw)].into_iter().flatten() {
        for server in source
            .get("mcp_servers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            push(server.get("name").and_then(Value::as_str));
        }
    }
    for tool in raw
        .pointer("/context_usage/mcpTools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        push(
            tool.get("serverName")
                .or_else(|| tool.get("server"))
                .and_then(Value::as_str),
        );
    }
    servers
}

/// What the probe found.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeReport {
    /// The detection the probe ran under.
    pub install: Option<ClaudeInstall>,
    /// The orchestrator session's init report.
    pub orchestrator: InitReport,
    /// The crewmate session's init report; its `tools` is the reported built-in tool list.
    pub crewmate: InitReport,
    /// The first refusal, if either session failed the check.
    pub check: Result<(), CheckFailure>,
    /// `initialize.models`, as detected models.
    pub models: Vec<DetectedModel>,
}

impl ProbeReport {
    /// The crewmate's reported tools (including `WebFetch`, `WebSearch`, `NotebookEdit`).
    pub fn crewmate_tools(&self) -> &[String] {
        &self.crewmate.tools
    }
}

/// Opens a zero-token orchestrator session and a zero-token crewmate session in a throwaway
/// folder (`$TMPDIR/quartermaster-probe-<pid>`), checks both, caches the model list, and closes
/// them. `tools` are offered to both.
pub async fn probe(
    engine: &ClaudeEngine,
    tools: Vec<ToolDescriptor>,
) -> Result<ProbeReport, EngineError> {
    let tmp = engine
        .config()
        .inherited_env
        .get("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let root = tmp.join(format!("quartermaster-probe-{}", std::process::id()));
    let orchestrator = probe_session(engine, &root, Role::Quartermaster, &tools).await?;
    let crewmate = probe_session(engine, &root, Role::Crewmate, &tools).await?;
    let check = check(
        &orchestrator,
        &ExpectedSession::new(Role::Quartermaster, &tools),
    )
    .and_then(|()| check(&crewmate, &ExpectedSession::new(Role::Crewmate, &tools)));
    let _ = std::fs::remove_dir_all(&root);
    Ok(ProbeReport {
        install: engine.install(),
        orchestrator,
        crewmate,
        check,
        models: engine.cached_models().unwrap_or_default(),
    })
}

async fn probe_session(
    engine: &ClaudeEngine,
    root: &std::path::Path,
    role: Role,
    tools: &[ToolDescriptor],
) -> Result<InitReport, EngineError> {
    let dir = root.join(role.as_str());
    let cwd = dir.join("cwd");
    let session_dir = dir.join("session");
    for path in [&cwd, &session_dir] {
        std::fs::create_dir_all(path).map_err(|e| {
            EngineError::Spawn(format!(
                "couldn't create the probe folder {}: {e}",
                path.display()
            ))
        })?;
    }
    let (prompt, builtins) = match role {
        Role::Crewmate => (
            SystemPrompt::AppendToClaudeCode {
                text: "Quartermaster probe session.".into(),
            },
            Builtins::ClaudeCode,
        ),
        Role::Quartermaster | Role::Secondmate => (
            SystemPrompt::Replace {
                text: "Quartermaster probe session.".into(),
            },
            Builtins::None,
        ),
    };
    let model = ModelId::new(PROBE_MODEL)
        .map_err(|e| EngineError::Protocol(format!("the probe model is invalid: {e}")))?;
    let mut spec = SessionSpec::new(
        AgentId::new(0),
        role,
        AccountRef {
            id: AccountId::new(0),
            provider: Provider::Claude,
            credential: None,
            base_url: None,
        },
        model,
        Effort::Low,
        cwd,
        session_dir,
        prompt,
    );
    spec.tools = tools.to_vec();
    spec.builtins = builtins;

    let mut handle = engine.open(spec)?;
    let clock = std::sync::Arc::clone(&engine.config().clock);
    let ready = tokio::select! {
        event = handle.next_event() => event,
        () = clock.sleep(PROBE_TIMEOUT) => None,
    };
    let report = match ready {
        Some(EngineEvent::SessionReady { init, .. }) => Ok(init),
        Some(EngineEvent::SessionClosed { reason }) => Err(EngineError::Unavailable(format!(
            "the Claude probe session didn't start: {reason:?}"
        ))),
        Some(other) => Err(EngineError::Protocol(format!(
            "the Claude probe session sent {} first",
            other.name()
        ))),
        None => Err(EngineError::Unavailable(
            "the Claude probe session didn't start within 90 seconds".into(),
        )),
    };
    let _ = handle.close();
    while let Some(event) = handle.next_event().await {
        if matches!(event, EngineEvent::SessionClosed { .. }) {
            break;
        }
    }
    report
}
