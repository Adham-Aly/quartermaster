//! A crewmate's session spec.
//!
//! | Field | Claude engine | Provider (and scripted) engine |
//! |---|---|---|
//! | `prompt` | appended to Claude Code's preset | the whole prompt |
//! | `builtins` | Claude Code's | our file tools, writing only to the workplace and evidence |
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
use std::path::PathBuf;
use std::time::Duration;

/// A crewmate's process is released after this long idle.
pub const CREW_IDLE_SHUTDOWN: Duration = Duration::from_mins(5);

/// Where a crewmate works: its worktree, or a folder task's folder, and whether its tools may write
/// there. An answer-only folder task reads its folder and writes only its evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Workplace {
    pub dir: PathBuf,
    pub writes_dir: bool,
}

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
    place: &Workplace,
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
        place.dir.clone(),
        paths.session.clone(),
        crewmate_system_prompt(routed.engine, gathered),
    );
    spec.resume = resume;
    spec.tools = qm_tools::registry::descriptors(Role::Crewmate);
    spec.builtins = match routed.engine {
        EngineKind::Claude => Builtins::ClaudeCode,
        EngineKind::Provider | EngineKind::Scripted => Builtins::ProviderFileTools {
            write_roots: if place.writes_dir {
                vec![place.dir.clone(), paths.evidence.clone()]
            } else {
                vec![paths.evidence.clone()]
            },
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
    use qm_types::ids::{AccountId, TaskId};
    use std::path::Path;

    fn routed() -> RoutedSession {
        RoutedSession {
            engine: EngineKind::Provider,
            account: AccountRef {
                id: AccountId::new(1),
                provider: Provider::ChatGpt,
                credential: None,
                base_url: None,
            },
            model: ModelId::new("gpt-test").expect("a model id"),
            effort: Effort::Low,
        }
    }

    /// A crewmate's spec for a workplace at `folder`, and the evidence folder it may write.
    fn spec_in(folder: &Path, writes_dir: bool) -> (SessionSpec, PathBuf) {
        let paths = DataDir::new("/qm-test/data").task(TaskId::new(1).expect("a task id"));
        let place = Workplace {
            dir: folder.to_path_buf(),
            writes_dir,
        };
        let spec = crewmate_spec(
            AgentId::new(2),
            &routed(),
            &paths,
            &place,
            PortBlock { base: 41020 },
            &Gathered::default(),
            None,
        );
        (spec, paths.evidence)
    }

    #[test]
    fn a_folder_crewmate_works_in_its_folder_and_may_write_there_and_its_evidence() {
        let folder = Path::new("/work/notes");
        let (spec, evidence) = spec_in(folder, true);
        assert_eq!(spec.cwd, folder);
        let Builtins::ProviderFileTools { write_roots } = spec.builtins else {
            panic!("the provider's file tools");
        };
        assert_eq!(write_roots, vec![folder.to_path_buf(), evidence]);
    }

    #[test]
    fn an_answer_only_folder_task_reads_its_folder_and_writes_only_its_evidence() {
        let folder = Path::new("/work/notes");
        let (spec, evidence) = spec_in(folder, false);
        assert_eq!(spec.cwd, folder);
        let Builtins::ProviderFileTools { write_roots } = spec.builtins else {
            panic!("the provider's file tools");
        };
        assert_eq!(write_roots, vec![evidence]);
    }
}
