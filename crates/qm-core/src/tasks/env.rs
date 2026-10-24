//! What a crewmate's processes find in their environment.
//!
//! Added to what each engine builds from scratch:
//!
//! - `QM_EVIDENCE_DIR`: the task's evidence folder, outside the worktree.
//! - `QM_REPORT_DIR`: the task's static report folder, outside the worktree.
//! - `QM_PORT_BASE`: the first of the task's ports.
//! - `QM_PLAN_DIR`: the task's plan folder in the scratch root, when it has one: the plan a
//!   planner writes, or the copy of the plan a build builds on.
//! - `GIT_TERMINAL_PROMPT=0`: a git command that needs a password fails instead of waiting for
//!   one.
//!
//! `gh` and git use the user's own GitHub login (`~/.config/gh`, the keychain, their credential
//! helpers), reached through `HOME`. No token variable ([`TOKEN_VARIABLES`]) and no key of
//! `NEVER_PASSED_TO_AGENTS` is ever set: the prompt keeps `pr_open` the default path, and the app
//! records what's done outside it (`delivery::outside`).

use qm_types::data_dir::TaskPaths;
use qm_types::env::{
    NEVER_PASSED_TO_AGENTS, QM_EVIDENCE_DIR, QM_PLAN_DIR, QM_PORT_BASE, QM_REPORT_DIR,
};
use qm_types::ids::PortBlock;
use std::collections::BTreeMap;
use std::path::Path;

/// Variables that would carry a GitHub token into a process; never set here.
pub const TOKEN_VARIABLES: [&str; 4] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// The crewmate's additions to its engine's environment.
pub fn crewmate_env(paths: &TaskPaths, ports: PortBlock) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        QM_EVIDENCE_DIR.to_owned(),
        absolute(&paths.evidence).to_string_lossy().into_owned(),
    );
    env.insert(
        QM_REPORT_DIR.to_owned(),
        absolute(&paths.artifacts.join("report"))
            .to_string_lossy()
            .into_owned(),
    );
    env.insert(QM_PORT_BASE.to_owned(), ports.base.to_string());
    if paths.plan.is_dir() {
        env.insert(
            QM_PLAN_DIR.to_owned(),
            absolute(&paths.plan).to_string_lossy().into_owned(),
        );
    }
    env.insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    debug_assert!(
        env.keys()
            .all(|k| !NEVER_PASSED_TO_AGENTS.contains(&k.as_str())
                && !TOKEN_VARIABLES.contains(&k.as_str())),
        "a crewmate's environment names a key it must never carry"
    );
    env
}

/// What the app's environment passes on to a setup command: the supervisor passes exactly what
/// it's given.
const INHERITED_FOR_SETUP: [&str; 8] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL",
];

/// The setup command's whole environment: the crewmate's additions plus the basics copied from
/// `inherited` (the app's own environment).
pub fn setup_env(
    paths: &TaskPaths,
    ports: PortBlock,
    inherited: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = crewmate_env(paths, ports);
    for key in INHERITED_FOR_SETUP {
        if let Some(value) = inherited.get(key) {
            env.insert(key.to_owned(), value.clone());
        }
    }
    env
}

fn absolute(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |d| d.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::data_dir::DataDir;
    use qm_types::ids::TaskId;

    #[test]
    fn report_folder_is_absolute_and_reaches_setup_too() {
        // Injection must not depend on the report directory already existing.
        let paths = DataDir::new("relative-report-env-test").task(
            TaskId::new(42).unwrap(),
            TaskId::new(42).unwrap().own_label(),
        );
        let ports = PortBlock { base: 41_000 };
        let env = crewmate_env(&paths, ports);
        let report = Path::new(env.get(QM_REPORT_DIR).unwrap());
        assert!(report.is_absolute());
        assert_eq!(report, absolute(&paths.artifacts.join("report")));
        assert_ne!(report, absolute(&paths.evidence));
        let setup = setup_env(&paths, ports, &BTreeMap::new());
        assert_eq!(setup.get(QM_REPORT_DIR), env.get(QM_REPORT_DIR));
    }

    #[test]
    fn a_crewmate_gets_the_users_github_login_and_no_token() {
        let dir = DataDir::new(std::env::temp_dir().join("qm-env-test"));
        let paths = dir.task(
            TaskId::new(42).expect("a task number"),
            TaskId::new(42).expect("a task number").own_label(),
        );
        let env = crewmate_env(&paths, PortBlock { base: 41_000 });
        assert_eq!(
            env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        for key in env.keys() {
            assert!(!key.starts_with("GIT_CONFIG"), "{key}");
            assert!(key != "GH_CONFIG_DIR" && key != "GIT_SSH_COMMAND", "{key}");
            assert!(!TOKEN_VARIABLES.contains(&key.as_str()), "{key}");
        }
    }
}
