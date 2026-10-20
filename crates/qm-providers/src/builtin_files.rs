//! The file and shell tools ([`FileTools`]) offered through the loop's [`BuiltinTools`] seam.
//!
//! [`FileToolsFactory`] is what the binary hands `ProviderEngineConfig::builtins`: one
//! [`FileTools`] per session, built from the session's [`BuiltinCx`]. The descriptors come from
//! the tool registry (`qm_tools::registry::for_role(Crewmate, Provider)`, the provider-only
//! tools), so the model sees exactly the names, descriptions and schemas `qm-tools` defines.
//!
//! Jobs: a `bash` command that outlives its 3 s wait becomes `J<n>`; its `Started` travels only
//! in the call's outcome ([`BuiltinOutcome::job_started`]) and the notice channel carries only
//! `Finished`, which the session reports as `JobFinished`, exactly once.

use crate::builtin::{BuiltinCx, BuiltinOutcome, BuiltinTools, BuiltinToolsFactory, JobNotice};
use crate::files::{FileToolOutcome, FileTools, FileToolsConfig, JobEvent};
use qm_engine::ToolDescriptor;
use qm_tools::EngineScope;
use qm_types::BoxFuture;
use qm_types::domain::{EngineKind, Role};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Makes each session's [`FileTools`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FileToolsFactory;

impl BuiltinToolsFactory for FileToolsFactory {
    fn create(&self, cx: BuiltinCx) -> Arc<dyn BuiltinTools> {
        let tools = FileTools::new(FileToolsConfig {
            agent: cx.agent,
            cwd: cx.cwd,
            write_roots: cx.write_roots,
            jobs_dir: cx.jobs_dir,
            env: cx.env,
            supervisor: cx.supervisor,
            clock: cx.clock,
        });
        Arc::new(SessionFileTools::new(tools))
    }
}

/// One session's file tools behind the seam.
#[derive(Debug)]
pub struct SessionFileTools {
    tools: FileTools,
    /// The notice channel, translated from the file tools' job events; handed out once.
    notices: Mutex<Option<mpsc::UnboundedReceiver<JobNotice>>>,
}

impl SessionFileTools {
    pub fn new(tools: FileTools) -> Self {
        let notices = tools.take_job_events().map(|mut events| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if tx.send(notice(event)).is_err() {
                        break;
                    }
                }
            });
            rx
        });
        Self {
            tools,
            notices: Mutex::new(notices),
        }
    }
}

fn notice(event: JobEvent) -> JobNotice {
    match event {
        JobEvent::Started {
            job,
            command,
            log_path,
        } => JobNotice::Started {
            job,
            command,
            log_path,
        },
        JobEvent::Finished {
            job,
            exit_code,
            tail,
        } => JobNotice::Finished {
            job,
            exit_code,
            tail,
        },
    }
}

fn outcome(o: FileToolOutcome) -> BuiltinOutcome {
    BuiltinOutcome {
        text: o.text,
        is_error: o.is_error,
        images: o.images,
        summary: o.summary,
        job_started: o.job_started.map(notice),
    }
}

impl BuiltinTools for SessionFileTools {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        qm_tools::registry::for_role(Role::Crewmate, EngineKind::Provider)
            .filter(|d| d.scope == EngineScope::ProviderOnly && self.tools.handles(d.name))
            .map(qm_tools::ToolDef::descriptor)
            .collect()
    }

    fn handles(&self, name: &str) -> bool {
        self.tools.handles(name)
    }

    fn run<'a>(&'a self, name: &'a str, args: serde_json::Value) -> BoxFuture<'a, BuiltinOutcome> {
        Box::pin(async move { outcome(self.tools.run(name, args).await) })
    }

    fn take_job_notices(&self) -> Option<mpsc::UnboundedReceiver<JobNotice>> {
        self.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn running_jobs(&self) -> usize {
        self.tools.running_jobs()
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.tools.shutdown())
    }
}
