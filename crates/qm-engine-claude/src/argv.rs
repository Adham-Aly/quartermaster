//! The command line, exactly as the Agent SDK builds it.
//!
//! The order and the `--flag=value` form follow the SDK's argv builder (`sdk.mjs`
//! `ProcessTransport.initialize`, 0.3.302), and only options we set appear.

use crate::SERVER_NAME;
use qm_engine::SessionSpec;
use qm_types::domain::Role;
use serde_json::json;
use std::path::PathBuf;

/// The only built-in tools a crewmate is offered, as the `--tools` allow-list. The rule: every tool
/// that acts only within the crewmate's own session or worktree, or reads the web, is granted;
/// anything that reaches other sessions, schedules, notifies, publishes, triggers remotely or
/// leaves the worktree is not (the `harness_tools!` list below).
///
/// - the coding tools: `Bash`, `Read`, `Edit`, `Write`, `NotebookEdit`, `Glob`, `Grep`;
/// - the web, up front: `WebFetch`, `WebSearch`;
/// - the skills the app hands over: `Skill`;
/// - its to-do list, kept in its own session: `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`;
/// - its own background work: `TaskStop` stops its own background shells and helper agents
///   (`KillShell` is an old name of it);
/// - helper agents, Claude Code's built-in types only: `Agent`, which `claude` reports as `Task`
///   ([`REPORTED_AS`]). A helper agent gets the crewmate's own tools and nothing more.
///
/// The session's every `system/init` is checked against this list (`check::check_tools`), so a
/// `claude` that offers more anyway is refused.
pub const CREWMATE_BUILTINS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "NotebookEdit",
    "Glob",
    "Grep",
    "WebFetch",
    "WebSearch",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskUpdate",
    "TaskStop",
    "Agent",
];

/// Built-in tools `claude` reports under another name than the one `--tools` takes:
/// `(granted, reported)`.
pub const REPORTED_AS: &[(&str, &str)] = &[("Agent", "Task")];

/// The names a crewmate's `system/init` may report for its built-ins: [`CREWMATE_BUILTINS`] and
/// their [`REPORTED_AS`] names.
pub fn crewmate_reported_builtins() -> Vec<&'static str> {
    let mut names = CREWMATE_BUILTINS.to_vec();
    for (granted, reported) in REPORTED_AS {
        if CREWMATE_BUILTINS.contains(granted) {
            names.push(reported);
        }
    }
    names
}

/// Claude Code's tools that reach outside an agent's task (claude 2.1.304's names):
/// messaging other sessions (`SendMessage`, `ListAgents`); scheduling (`CronCreate`, `CronDelete`,
/// `CronList`, `ScheduleWakeup`); notifying (`PushNotification`); publishing (`DesignSync`,
/// `ReportFindings`, the `Artifact` family); remote triggers and agent swarms (`RemoteTrigger`,
/// `Workflow`); leaving the worktree (`EnterWorktree`, `ExitWorktree`); and `Monitor`, which wakes
/// the session by itself on a process's output, outside the app's wakes (background jobs already
/// wake a crewmate through the app). Disallowed for every role, in case a `claude` offers one
/// outside `--tools`.
macro_rules! harness_tools {
    () => {
        "SendMessage,ListAgents,CronCreate,CronDelete,CronList,ScheduleWakeup,PushNotification,\
         DesignSync,ReportFindings,Artifact,ArtifactComments,ArtifactData,RemoteTrigger,Workflow,\
         EnterWorktree,ExitWorktree,Monitor"
    };
}

/// The built-in tools turned off for orchestrators: they have no built-in tools at all
/// (`--tools=`); these are disallowed as well, so none can come back by another route: the
/// harness tools, helper agents, background-task control and lazy tool loading.
pub const ORCHESTRATOR_DISALLOWED: &str =
    concat!(harness_tools!(), ",Agent,Task,TaskStop,ToolSearch");

/// The built-in tools turned off for crewmates, beyond the allow-list: the harness tools, tools
/// that wait on a person who isn't there (`ask_decision` is the crewmate's way to ask) and lazy
/// tool loading (`ENABLE_TOOL_SEARCH=false` already makes `claude` defer nothing).
pub const CREWMATE_DISALLOWED: &str = concat!(
    harness_tools!(),
    ",AskUserQuestion,EnterPlanMode,ExitPlanMode,ToolSearch"
);

/// The `--settings` value (the SDK's `extraArgs.settings`): Claude Code's own commit and PR
/// attribution off, the autocompact window, and autocompact on, so the user's own `/autocompact`
/// toggle can't turn it off for our agents. Claude Code caps the window at the model's own.
pub fn settings_json(autocompact_window: u32) -> String {
    json!({
        "attribution": {"commit": "", "pr": ""},
        "autoCompactEnabled": true,
        "autoCompactWindow": autocompact_window,
    })
    .to_string()
}

/// Which tool and permission set the session gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolSet {
    /// The Quartermaster and secondmates: our tools only, `dontAsk`.
    Orchestrator,
    /// Crewmates: the granted built-ins ([`CREWMATE_BUILTINS`]) plus ours, permission prompts
    /// off.
    Crewmate,
}

impl ToolSet {
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::Quartermaster | Role::Secondmate => Self::Orchestrator,
            Role::Crewmate => Self::Crewmate,
        }
    }

    /// The permission mode `claude` reports for this set.
    pub const fn permission_mode(self) -> &'static str {
        match self {
            Self::Orchestrator => "dontAsk",
            Self::Crewmate => "bypassPermissions",
        }
    }
}

/// The options that shape the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchOptions {
    pub model: String,
    /// `--effort=`, the agent's fixed effort level (wire name).
    pub effort: Option<String>,
    /// `--resume=<session id>`.
    pub resume: Option<String>,
    /// `--session-id=<uuid v4>` for a new session (never with `resume`).
    pub session_id: Option<String>,
    pub tools: ToolSet,
    /// `--plugin-dir-no-mcp=`: the skills-only plugin folder (crewmates with skills).
    pub plugin_dir: Option<PathBuf>,
    /// `--settings <json>`, two arguments, last: [`settings_json`].
    pub settings: Option<String>,
}

impl LaunchOptions {
    /// The options production uses for a session: a new one gets `session_id`, a resumed one
    /// `--resume` with `resume_id`. `autocompact_window` is the setting in force at launch.
    pub fn for_session(
        spec: &SessionSpec,
        model: &str,
        session_id: &str,
        resuming: bool,
        plugin_dir: Option<PathBuf>,
        autocompact_window: u32,
    ) -> Self {
        Self {
            model: model.to_owned(),
            effort: Some(spec.effort.as_str().to_owned()),
            resume: resuming.then(|| session_id.to_owned()),
            session_id: (!resuming).then(|| session_id.to_owned()),
            tools: ToolSet::for_role(spec.role),
            plugin_dir,
            settings: Some(settings_json(autocompact_window)),
        }
    }
}

/// The argv, in the SDK's order.
pub fn build_argv(options: &LaunchOptions) -> Vec<String> {
    let mut argv: Vec<String> = [
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some(effort) = &options.effort {
        argv.push(format!("--effort={effort}"));
    }
    argv.push(format!("--model={}", options.model));
    if let Some(resume) = &options.resume {
        argv.push(format!("--resume={resume}"));
    }
    argv.push(format!("--allowedTools=mcp__{SERVER_NAME}__*"));
    match options.tools {
        ToolSet::Orchestrator => {
            argv.push(format!("--disallowedTools={ORCHESTRATOR_DISALLOWED}"));
            argv.push("--tools=".to_owned());
        }
        ToolSet::Crewmate => {
            argv.push(format!("--disallowedTools={CREWMATE_DISALLOWED}"));
            argv.push(format!("--tools={}", CREWMATE_BUILTINS.join(",")));
        }
    }
    argv.push("--setting-sources=".to_owned());
    argv.push("--strict-mcp-config".to_owned());
    match options.tools {
        ToolSet::Orchestrator => argv.push("--permission-mode=dontAsk".to_owned()),
        ToolSet::Crewmate => {
            argv.push("--permission-mode=bypassPermissions".to_owned());
            argv.push("--allow-dangerously-skip-permissions".to_owned());
        }
    }
    argv.push("--include-partial-messages".to_owned());
    if let Some(dir) = &options.plugin_dir {
        argv.push(format!("--plugin-dir-no-mcp={}", dir.display()));
    }
    if options.resume.is_none()
        && let Some(id) = &options.session_id
    {
        argv.push(format!("--session-id={id}"));
    }
    if let Some(settings) = &options.settings {
        argv.push("--settings".to_owned());
        argv.push(settings.clone());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(window: u32) -> LaunchOptions {
        LaunchOptions {
            model: "claude-sonnet-5-5".into(),
            effort: None,
            resume: None,
            session_id: Some("s".into()),
            tools: ToolSet::Orchestrator,
            plugin_dir: None,
            settings: Some(settings_json(window)),
        }
    }

    #[test]
    fn settings_json_carries_attribution_and_the_window() {
        for window in [100_000, 200_000, 1_000_000] {
            let value: serde_json::Value =
                serde_json::from_str(&settings_json(window)).expect("valid JSON");
            assert_eq!(
                value,
                json!({
                    "attribution": {"commit": "", "pr": ""},
                    "autoCompactEnabled": true,
                    "autoCompactWindow": window,
                })
            );
        }
        assert_eq!(
            settings_json(200_000),
            r#"{"attribution":{"commit":"","pr":""},"autoCompactEnabled":true,"autoCompactWindow":200000}"#
        );
    }

    #[test]
    fn settings_stay_last_and_the_sources_stay_empty() {
        let argv = build_argv(&options(167_000));
        let n = argv.len();
        assert_eq!(argv[n - 2], "--settings");
        assert_eq!(argv[n - 1], settings_json(167_000));
        assert!(argv.iter().any(|a| a == "--setting-sources="));
    }
}
