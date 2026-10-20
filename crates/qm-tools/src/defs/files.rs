//! The crewmate's file and shell tools on the provider engine.
//!
//! Defined here so their descriptions and schemas live with every other tool's;
//! implemented in `qm-providers`, which runs them inside the engine (they never come back to the
//! core) and offers them when a session asks for `Builtins::ProviderFileTools`. Names, arguments
//! and behaviour follow Claude Code's tools closely, so a prompt reads the same on both engines.
//! Results are plain text, as Claude Code's are.

use qm_types::ids::JobId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Defines a provider-engine file or shell tool: crewmates only, plain-text results.
macro_rules! file_tool {
    (
        $(#[$meta:meta])*
        $marker:ident { name: $name:literal, class: $class:ident, args: $args:ty $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $marker;

        impl $crate::Tool for $marker {
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/descriptions/",
                $name,
                ".md"
            ))
            .trim_ascii();
            const CLASS: $crate::ToolClass = $crate::ToolClass::$class;
            const ROLES: &'static [qm_types::domain::Role] = &[qm_types::domain::Role::Crewmate];
            const SCOPE: $crate::EngineScope = $crate::EngineScope::ProviderOnly;
            type Args = $args;
            type Output = String;

            fn render(output: &String) -> String {
                output.clone()
            }
        }
    };
}

file_tool! {
    /// Reads a file with line numbers, ranges and images.
    ReadTool { name: "read", class: Read, args: ReadArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

file_tool! {
    /// Writes a file; refused outside the worktree, the evidence folder and the temporary folder.
    WriteTool { name: "write", class: Act, args: WriteArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    pub file_path: String,
    pub content: String,
}

file_tool! {
    /// An exact text replacement that must match once unless `replace_all`.
    EditTool { name: "edit", class: Act, args: EditArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace_all: bool,
}

file_tool! {
    /// Searches contents with ripgrep's libraries, respecting `.gitignore`.
    GrepTool { name: "grep", class: Read, args: GrepArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<GrepOutputMode>,
    #[serde(rename = "-i", default, skip_serializing_if = "Option::is_none")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "-n", default, skip_serializing_if = "Option::is_none")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-A", default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u32>,
    #[serde(rename = "-B", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u32>,
    #[serde(rename = "-C", default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiline: Option<bool>,
}

// What grep returns: matching lines, the files that match (the default), or counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    Content,
    FilesWithMatches,
    Count,
}

file_tool! {
    /// Finds files by glob, respecting `.gitignore` (Claude Code's `Glob`).
    FindTool { name: "find", class: Read, args: FindArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindArgs {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

file_tool! {
    /// Lists a folder.
    LsTool { name: "ls", class: Read, args: LsArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LsArgs {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

file_tool! {
    /// Runs a shell command; returns within about 3 seconds or becomes a background job.
    BashTool { name: "bash", class: Act, args: BashArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    pub command: String,
    // Milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run_in_background: bool,
}

file_tool! {
    /// Reads a background job's output.
    JobOutputTool { name: "job_output", class: Read, args: JobOutputArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobOutputArgs {
    pub job: JobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

file_tool! {
    /// Stops a background job's whole process group.
    JobKillTool { name: "job_kill", class: Act, args: JobKillArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobKillArgs {
    pub job: JobId,
}

file_tool! {
    /// Starts a helper agent: a fresh context on the crewmate's account and model, in its
    /// worktree, with the file and shell tools above but no `agent` and no tool-host tools; its
    /// final message is the result (like Claude Code's `Agent`).
    AgentTool { name: "agent", class: Act, args: AgentArgs }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentArgs {
    // Three to six words, shown as the tool line.
    pub description: String,
    // The whole job, self-contained: the helper sees nothing else.
    pub prompt: String,
}
