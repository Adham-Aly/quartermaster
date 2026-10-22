//! Environment variable names the app reads or sets.

/// Overrides the data directory, `~/.quartermaster` by default. Every development and test run sets
/// it to a throwaway directory.
pub const QUARTERMASTER_HOME: &str = "QUARTERMASTER_HOME";

/// Set for a crewmate: its task's evidence folder, outside the worktree.
pub const QM_EVIDENCE_DIR: &str = "QM_EVIDENCE_DIR";

/// Set for a crewmate: the first of its task's block of ports.
pub const QM_PORT_BASE: &str = "QM_PORT_BASE";

/// Set for a crewmate whose task has a plan folder: the plan it writes, or the plan it builds on.
pub const QM_PLAN_DIR: &str = "QM_PLAN_DIR";

/// Variables the app never sets for, or passes to, any agent process: a stray key in your shell
/// must not move Claude agents onto per-token billing. `CLAUDE_CONFIG_DIR` is left alone entirely,
/// because changing it logs subscription users out on macOS.
pub const NEVER_PASSED_TO_AGENTS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];
