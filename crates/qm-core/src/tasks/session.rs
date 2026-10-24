//! A crewmate's session spec.
//!
//! | Field | Claude engine | Provider (and scripted) engine |
//! |---|---|---|
//! | `prompt` | appended to Claude Code's preset | the whole prompt |
//! | `builtins` | Claude Code's | our file tools, writing anywhere but the app's data and scratch folders |
//! | `env` | `env::crewmate_env` | same |
//! | `idle_shutdown` | 5 minutes | same |
//! | `limits` | none: crewmates have no limits of ours | same |

use super::env::crewmate_env;
use crate::briefs::crewmate_system_prompt;
use crate::skills::Gathered;
use qm_engine::{AccountRef, Builtins, SessionSpec};
use qm_types::data_dir::TaskPaths;
use qm_types::domain::{Effort, EngineKind, PlanMode, Role};
use qm_types::ids::{AgentId, ModelId, PortBlock, SessionRef};
use std::path::PathBuf;
use std::time::Duration;

/// A crewmate's process is released after this long idle.
pub const CREW_IDLE_SHUTDOWN: Duration = Duration::from_mins(5);

/// Where a crewmate works: its worktree, or the folder an in-place task works in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Workplace {
    pub dir: PathBuf,
}

/// What routing decided, as the session needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutedSession {
    pub engine: EngineKind,
    pub account: AccountRef,
    pub model: ModelId,
    pub effort: Effort,
}

/// What the crewmate is told: its skills and, for a plan task, the plan mode its session runs in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Told<'a> {
    pub gathered: &'a Gathered,
    pub plan: Option<PlanMode>,
}

/// The crewmate's spec.
pub(crate) fn crewmate_spec(
    agent: AgentId,
    routed: &RoutedSession,
    paths: &TaskPaths,
    place: &Workplace,
    ports: PortBlock,
    told: Told<'_>,
    resume: Option<SessionRef>,
) -> SessionSpec {
    let Told { gathered, plan } = told;
    let mut spec = SessionSpec::new(
        agent,
        Role::Crewmate,
        routed.account.clone(),
        routed.model.clone(),
        routed.effort,
        place.dir.clone(),
        paths.session.clone(),
        crewmate_system_prompt(routed.engine, gathered, plan),
    );
    spec.resume = resume;
    if routed.engine != EngineKind::Claude {
        spec.transcript_dir = Some(paths.transcript.clone());
    }
    spec.tools = qm_tools::registry::descriptors(Role::Crewmate, plan);
    spec.builtins = match routed.engine {
        EngineKind::Claude => Builtins::ClaudeCode,
        EngineKind::Provider | EngineKind::Scripted => Builtins::ProviderFileTools {
            write_roots: {
                // Folders inside the app's own that stay writable.
                let mut roots = vec![
                    place.dir.clone(),
                    paths.evidence.clone(),
                    paths.artifacts.join("report"),
                ];
                // A planner writes its plan; a build reads its copy of the plan it builds on.
                if plan.is_some() {
                    roots.push(paths.plan.clone());
                }
                roots
            },
            scratchpad: Some(paths.scratchpad.clone()),
        },
    };
    spec.skills_dir.clone_from(&gathered.skills_dir);
    spec.env = crewmate_env(paths, ports);
    spec.idle_shutdown = Some(CREW_IDLE_SHUTDOWN);
    spec.limits = None;
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::data_dir::DataDir;
    use qm_types::domain::Provider;
    use qm_types::env::QM_REPORT_DIR;
    use qm_types::ids::{AccountId, TaskId};

    #[test]
    fn report_access_and_environment_are_available_on_every_engine() {
        let paths = DataDir::new(PathBuf::from("/nonexistent/qm-session-test")).task(
            TaskId::new(42).unwrap(),
            TaskId::new(42).unwrap().own_label(),
        );
        let place = Workplace {
            dir: paths.worktree.clone(),
        };
        let gathered = Gathered::default();
        for engine in [
            EngineKind::Claude,
            EngineKind::Provider,
            EngineKind::Scripted,
        ] {
            let routed = RoutedSession {
                engine,
                account: AccountRef {
                    id: AccountId::new(1),
                    provider: Provider::Scripted,
                    credential: None,
                    base_url: None,
                },
                model: ModelId::new("test-model").unwrap(),
                effort: Effort::Medium,
            };
            for plan in [None, Some(PlanMode::Internal), Some(PlanMode::Interactive)] {
                let spec = crewmate_spec(
                    AgentId::new(1),
                    &routed,
                    &paths,
                    &place,
                    PortBlock { base: 41_000 },
                    Told {
                        gathered: &gathered,
                        plan,
                    },
                    None,
                );
                assert_eq!(
                    spec.env.get(QM_REPORT_DIR).unwrap(),
                    &paths.artifacts.join("report").to_string_lossy(),
                );
                match spec.builtins {
                    Builtins::ClaudeCode => assert_eq!(engine, EngineKind::Claude),
                    Builtins::ProviderFileTools { write_roots, .. } => {
                        assert!(write_roots.contains(&paths.artifacts.join("report")));
                        assert!(write_roots.contains(&paths.evidence));
                        assert!(!write_roots.contains(&paths.artifacts));
                        assert_eq!(write_roots.contains(&paths.plan), plan.is_some());
                    }
                    Builtins::None => panic!("a crewmate needs builtins"),
                }
            }
        }
    }
}
