//! The seam the loop runs built-in tools through.
//!
//! The provider engine offers crewmates our file and shell tools (`read`, `write`, `edit`,
//! `grep`, `find`, `ls`, `bash`, `job_output`, `job_kill`). They run inside the
//! engine, not in the core, so the loop calls them through [`BuiltinTools`]; the binary hands the
//! engine a [`BuiltinToolsFactory`] (`files::FileTools` behind `builtin_files.rs`). With
//! no factory, a spec asking for `Builtins::ProviderFileTools` is refused as unsupported.
//!
//! Background jobs (`bash` past 3 s) come back as [`JobNotice`]s: `Started` in the outcome of the
//! call that started the job, `Finished` on the notice channel whenever the job ends, between
//! turns too. The session turns them into `JobStarted`/`JobFinished`, exactly one each.

use qm_engine::supervisor::Supervisor;
use qm_engine::{ToolDescriptor, TurnUsage};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::ids::{AgentId, JobLabel, ModelId};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Makes one session's built-in tools.
pub trait BuiltinToolsFactory: Send + Sync + fmt::Debug {
    fn create(&self, cx: BuiltinCx) -> Arc<dyn BuiltinTools>;
}

/// What one session's built-in tools need.
#[derive(Clone)]
pub struct BuiltinCx {
    pub agent: AgentId,
    /// The worktree: relative paths resolve here, `bash` runs here.
    pub cwd: PathBuf,
    /// Writes are refused outside these (and the system temporary folder).
    pub write_roots: Vec<PathBuf>,
    /// The app's own folders (its data folder): nothing inside them is writable except through a
    /// write root strictly inside one.
    pub protected_roots: Vec<PathBuf>,
    /// `tasks/C-0042/jobs`: job logs, `J<n>.log`.
    pub jobs_dir: PathBuf,
    /// The complete environment for `bash`, built from scratch.
    pub env: BTreeMap<String, String>,
    /// Starts and stops `bash`'s processes.
    pub supervisor: Arc<dyn Supervisor>,
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for BuiltinCx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltinCx")
            .field("agent", &self.agent)
            .field("cwd", &self.cwd)
            .field("write_roots", &self.write_roots)
            .field("protected_roots", &self.protected_roots)
            .field("jobs_dir", &self.jobs_dir)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// One session's built-in tools.
pub trait BuiltinTools: Send + Sync + fmt::Debug {
    /// What to offer the model.
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    /// Whether `name` is one of these tools.
    fn handles(&self, name: &str) -> bool;

    /// Runs one call. Dropping the future cancels it (a foreground command's group is stopped).
    fn run<'a>(&'a self, name: &'a str, args: serde_json::Value) -> BoxFuture<'a, BuiltinOutcome>;

    /// The job notice channel; `Some` once, `None` after.
    fn take_job_notices(&self) -> Option<mpsc::UnboundedReceiver<JobNotice>>;

    /// How many background jobs are running.
    fn running_jobs(&self) -> usize;

    /// Stops every job; their `Finished` notices follow on the channel.
    fn shutdown(&self) -> BoxFuture<'_, ()>;

    /// Model usage these tools spent since the last call (helper agents' calls, which count
    /// toward the crewmate's turn); the loop adds it to the turn's usage when the turn ends.
    fn take_usage(&self) -> TurnUsage {
        TurnUsage::default()
    }

    /// The session switched to `model` (`SetModel`); helper agents follow it.
    fn model_changed(&self, _model: &ModelId) {}
}

/// What one built-in call returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltinOutcome {
    /// The result text for the model.
    pub text: String,
    pub is_error: bool,
    /// Images for the model, as `(media type, bytes)` (`read` of a picture).
    pub images: Vec<(String, Vec<u8>)>,
    /// One line for the worker session view ("cargo test auth").
    pub summary: String,
    /// The job this call started, if it became one.
    pub job_started: Option<JobNotice>,
}

/// A background job's news.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobNotice {
    Started {
        job: JobLabel,
        command: String,
        log_path: PathBuf,
    },
    Finished {
        job: JobLabel,
        exit_code: Option<i32>,
        /// The last lines of its output.
        tail: String,
    },
}

/// A one-line summary of a built-in call's arguments, for `BuiltinToolUse` before the call runs:
/// `bash` → its command, file tools → the file path, search tools → the pattern, `agent` → its
/// description, else the first string argument; cut to 120 characters.
pub fn summarize_args(name: &str, args: &serde_json::Value) -> String {
    let field = match name {
        "bash" => Some("command"),
        "read" | "write" | "edit" => Some("file_path"),
        "ls" => Some("path"),
        "grep" | "find" => Some("pattern"),
        "job_output" | "job_kill" => Some("job"),
        "agent" => Some("description"),
        _ => None,
    };
    let text = field
        .and_then(|f| args.get(f))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            args.as_object()?
                .values()
                .find_map(serde_json::Value::as_str)
        })
        .unwrap_or_default();
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 120 {
        let mut cut: String = line.chars().take(119).collect();
        cut.push('…');
        cut
    } else {
        line.to_owned()
    }
}
