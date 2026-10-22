//! What a crewmate's processes find in their environment.
//!
//! Added to what each engine builds from scratch:
//!
//! - `QM_EVIDENCE_DIR`: the task's evidence folder, outside the worktree.
//! - `QM_PORT_BASE`: the first of the task's ports.
//! - `QM_PLAN_DIR`: the task's plan folder in the scratch root, when it has one: the plan a
//!   planner writes, or the copy of the plan a build builds on.
//! - `GH_CONFIG_DIR`: an empty folder, so `gh` is logged out.
//! - The push fence, `qm_github::fence::apply`, in layers (see there): every https, http and ssh
//!   push URL and `origin`'s push URL go to `/dev/null/pushes go through pr_open/…`, which isn't a
//!   repository, and ssh refuses a push to any host, so `git push` fails with that sentence
//!   while fetching is untouched.
//!
//! `GH_TOKEN`, `GITHUB_TOKEN` and every key of `NEVER_PASSED_TO_AGENTS` are never set.

use qm_github::fence;
use qm_types::data_dir::TaskPaths;
use qm_types::env::{NEVER_PASSED_TO_AGENTS, QM_EVIDENCE_DIR, QM_PLAN_DIR, QM_PORT_BASE};
use qm_types::ids::PortBlock;
use std::collections::BTreeMap;
use std::path::Path;

/// The folder `gh` reads its login from: `tasks/T-0042/gh`, kept empty.
pub fn gh_dir(paths: &TaskPaths) -> std::path::PathBuf {
    paths.root.join("gh")
}

/// Where a pushed GitHub URL goes instead (`qm_github::fence::PUSH_FENCE_PREFIX`).
pub const PUSH_FENCE: &str = fence::PUSH_FENCE_PREFIX;

/// Keys that must never reach an agent from here.
const FORBIDDEN: [&str; 4] = fence::TOKEN_VARIABLES;

/// The crewmate's additions to its engine's environment.
pub fn crewmate_env(paths: &TaskPaths, ports: PortBlock) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        QM_EVIDENCE_DIR.to_owned(),
        absolute(&paths.evidence).to_string_lossy().into_owned(),
    );
    env.insert(QM_PORT_BASE.to_owned(), ports.base.to_string());
    if paths.plan.is_dir() {
        env.insert(
            QM_PLAN_DIR.to_owned(),
            absolute(&paths.plan).to_string_lossy().into_owned(),
        );
    }
    // The GitHub fence: its layers are `qm_github::fence`'s, and the result is held to them before
    // any crewmate process gets it.
    fence::apply(&mut env, &absolute(&gh_dir(paths)).to_string_lossy());
    if let Err(why) = fence::check(&env) {
        tracing::error!(%why, "a crewmate's environment doesn't hold the GitHub fence");
        debug_assert!(false, "the GitHub fence is broken: {why}");
    }
    debug_assert!(
        env.keys()
            .all(|k| !NEVER_PASSED_TO_AGENTS.contains(&k.as_str())
                && !FORBIDDEN.contains(&k.as_str())),
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
