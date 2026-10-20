//! A crewmate's session spec.
//!
//! | Field | Claude engine | Provider (and scripted) engine |
//! |---|---|---|
//! | `prompt` | appended to Claude Code's preset | the whole prompt |
//! | `builtins` | Claude Code's | our file tools, writing only to the worktree and evidence |
//! | `env` | `env::crewmate_env` | same |
//! | `idle_shutdown` | 5 minutes | same |
//! | `limits` | none: crewmates have no limits of ours | same |

use super::env::crewmate_env;
use crate::briefs::crewmate_system_prompt;
use crate::skills::Gathered;
use qm_engine::{AccountRef, Builtins, SessionSpec};
use qm_types::data_dir::TaskPaths;
use qm_types::domain::{Effort, EngineKind, Role};
use qm_types::ids::{AgentId, ModelId, PortBlock, SessionRef};
use std::time::Duration;

/// A crewmate's process is released after this long idle.
pub const CREW_IDLE_SHUTDOWN: Duration = Duration::from_mins(5);

/// What routing decided, as the session needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutedSession {
    pub engine: EngineKind,
    pub account: AccountRef,
    pub model: ModelId,
    pub effort: Effort,
}

/// The crewmate's spec.
pub(crate) fn crewmate_spec(
    agent: AgentId,
    routed: &RoutedSession,
    paths: &TaskPaths,
    ports: PortBlock,
    gathered: &Gathered,
    resume: Option<SessionRef>,
) -> SessionSpec {
    let mut spec = SessionSpec::new(
        agent,
        Role::Crewmate,
        routed.account.clone(),
        routed.model.clone(),
        routed.effort,
        paths.worktree.clone(),
        paths.session.clone(),
        crewmate_system_prompt(routed.engine, gathered),
    );
    spec.resume = resume;
    spec.tools = qm_tools::registry::descriptors(Role::Crewmate);
    spec.builtins = match routed.engine {
        EngineKind::Claude => Builtins::ClaudeCode,
        EngineKind::Provider | EngineKind::Scripted => Builtins::ProviderFileTools {
            write_roots: vec![paths.worktree.clone(), paths.evidence.clone()],
        },
    };
    spec.skills_dir.clone_from(&gathered.skills_dir);
    spec.env = crewmate_env(paths, ports);
    spec.idle_shutdown = Some(CREW_IDLE_SHUTDOWN);
    spec.limits = None;
    spec
}
