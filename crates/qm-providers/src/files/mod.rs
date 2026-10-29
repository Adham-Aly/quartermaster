//! The crewmate's file and shell tools on the provider engine.
//!
//! `read`, `write`, `edit`, `grep`, `find`, `ls`, `bash`, `job_output` and `job_kill`, following
//! Claude Code's tools of the same purpose so a prompt reads the same on both engines. Their
//! argument types and descriptions live in `qm_tools::defs::files`; this module runs them inside
//! the engine (they never go back to the core). Results are plain text.
//!
//! - **Paths**: a relative path resolves against the worktree. Reads are allowed anywhere, and so
//!   are writes, except into the app's own state (its data and scratch folders, where only the
//!   caller's write roots and any task's worktree are open), checked after resolving symlinks and
//!   `..` (`paths`).
//! - **Read before overwrite**: `write` over an existing file and `edit` need a `read` of that file
//!   in this session, and refuse if it changed since.
//! - **`grep` and `find`** walk with ripgrep's own libraries, so `.gitignore` is respected.
//! - **`bash`** runs through the [`Supervisor`] in its own process group, logging to
//!   `jobs/J<n>.log`. A command still running after about 3 seconds becomes background job `J<n>`;
//!   its exit is reported on the channel from [`FileTools::take_job_events`] as
//!   [`JobEvent::Finished`]. The `Started` event travels in the tool's outcome
//!   ([`FileToolOutcome::job_started`]), so the engine can report it inside the turn, before the
//!   tool's result.
//!
//! The provider engine offers these through its built-in tools seam (`builtin_files.rs`).

mod bash;
mod edit;
mod find;
mod grep;
mod jobs;
mod ls;
mod paths;
mod read;
mod write;

use jobs::JobTable;
use paths::WritePolicy;
use qm_engine::supervisor::Supervisor;
use qm_tools::defs::files::{
    BashArgs, EditArgs, FindArgs, GrepArgs, JobKillArgs, JobOutputArgs, LsArgs, ReadArgs, WriteArgs,
};
use qm_types::clock::Clock;
use qm_types::ids::{AgentId, JobLabel};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;
use tokio::sync::mpsc;

/// The system prompt section about an agent's scratchpad, mirroring what Claude Code tells its
/// models: use this folder for every temporary file instead of `/tmp`.
#[must_use]
pub fn scratchpad_section(dir: &std::path::Path) -> String {
    format!(
        "\n\n{}\n",
        include_str!("scratchpad.md")
            .trim_end()
            .replace("{dir}", &dir.display().to_string())
    )
}

/// What a session's file tools work with.
#[derive(Debug, Clone)]
pub struct FileToolsConfig {
    pub agent: AgentId,
    /// The worktree: relative paths resolve here, and `bash` runs here.
    pub cwd: PathBuf,
    /// Where writes are allowed besides the system temporary folder (from
    /// `Builtins::ProviderFileTools`: the worktree and the evidence folder).
    pub write_roots: Vec<PathBuf>,
    /// The app's data folder and anything else no write may reach except through a write root
    /// strictly inside it.
    pub protected_roots: Vec<PathBuf>,
    /// The task's `jobs/` folder, where every `bash` command logs to `J<n>.log`.
    pub jobs_dir: PathBuf,
    /// The complete environment for `bash`, built by the engine from scratch.
    pub env: BTreeMap<String, String>,
    pub supervisor: Arc<dyn Supervisor>,
    pub clock: Arc<dyn Clock>,
}

/// What one tool call returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileToolOutcome {
    /// The plain-text result for the model.
    pub text: String,
    pub is_error: bool,
    /// Images for the model, as (media type, bytes): `read` of a PNG, JPEG, GIF or WebP file.
    pub images: Vec<(String, Vec<u8>)>,
    /// A one-line summary for the worker session view ("▸ bash cargo test"): the command, path or
    /// pattern, cut to 120 characters.
    pub summary: String,
    /// Set when `bash` left a background job running: the engine reports `JobStarted` before the
    /// tool's result.
    pub job_started: Option<JobEvent>,
}

impl FileToolOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            images: Vec::new(),
            summary: String::new(),
            job_started: None,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::ok(text)
        }
    }
}

/// A background job's life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Started {
        job: JobLabel,
        command: String,
        log_path: PathBuf,
    },
    /// Exactly one per started job. `exit_code` is `None` when the job was stopped (by `job_kill`
    /// or shutdown) or died from a signal. `tail` is its last 20 lines, at most 2,000 characters.
    Finished {
        job: JobLabel,
        exit_code: Option<i32>,
        tail: String,
    },
}

/// One session's file and shell tools.
#[derive(Debug)]
pub struct FileTools {
    config: FileToolsConfig,
    policy: WritePolicy,
    /// Files read in this session, by canonical path, with the modification time seen then.
    reads: Mutex<HashMap<PathBuf, SystemTime>>,
    jobs: Arc<JobTable>,
    job_events: Mutex<Option<mpsc::UnboundedReceiver<JobEvent>>>,
}

impl FileTools {
    /// Every tool name this module handles.
    pub const NAMES: &'static [&'static str] = &[
        "read",
        "write",
        "edit",
        "grep",
        "find",
        "ls",
        "bash",
        "job_output",
        "job_kill",
    ];

    pub fn new(config: FileToolsConfig) -> Self {
        let policy = WritePolicy::new(&config.write_roots, &config.protected_roots);
        let (tx, rx) = mpsc::unbounded_channel();
        let jobs = Arc::new(JobTable::new(tx));
        Self {
            config,
            policy,
            reads: Mutex::new(HashMap::new()),
            jobs,
            job_events: Mutex::new(Some(rx)),
        }
    }

    pub fn handles(&self, name: &str) -> bool {
        Self::NAMES.contains(&name)
    }

    /// Runs one tool. Dropping the future cancels it; a foreground `bash` command is then
    /// stopped.
    pub async fn run(&self, name: &str, args: serde_json::Value) -> FileToolOutcome {
        let mut outcome = match name {
            "read" => match parse::<ReadArgs>(name, args) {
                Ok(a) => self.with_summary(&a.file_path, read::run(self, &a)),
                Err(e) => *e,
            },
            "write" => match parse::<WriteArgs>(name, args) {
                Ok(a) => self.with_summary(&a.file_path, write::run(self, &a)),
                Err(e) => *e,
            },
            "edit" => match parse::<EditArgs>(name, args) {
                Ok(a) => self.with_summary(&a.file_path, edit::run(self, &a)),
                Err(e) => *e,
            },
            "grep" => match parse::<GrepArgs>(name, args) {
                Ok(a) => self.with_summary(&a.pattern, grep::run(self, &a)),
                Err(e) => *e,
            },
            "find" => match parse::<FindArgs>(name, args) {
                Ok(a) => self.with_summary(&a.pattern, find::run(self, &a)),
                Err(e) => *e,
            },
            "ls" => match parse::<LsArgs>(name, args) {
                Ok(a) => self.with_summary(&a.path, ls::run(self, &a)),
                Err(e) => *e,
            },
            "bash" => match parse::<BashArgs>(name, args) {
                Ok(a) => {
                    let out = bash::run(self, &a).await;
                    self.with_summary(&a.command, out)
                }
                Err(e) => *e,
            },
            "job_output" => match parse::<JobOutputArgs>(name, args) {
                Ok(a) => {
                    let out = self.jobs.output(a.job, a.filter.as_deref());
                    self.with_summary(&a.job.to_string(), out)
                }
                Err(e) => *e,
            },
            "job_kill" => match parse::<JobKillArgs>(name, args) {
                Ok(a) => {
                    let out = self.jobs.kill(&self.config, a.job).await;
                    self.with_summary(&a.job.to_string(), out)
                }
                Err(e) => *e,
            },
            other => FileToolOutcome::error(format!("unknown tool {other}")),
        };
        if outcome.summary.is_empty() {
            outcome.summary = summary(name);
        }
        outcome
    }

    /// The channel of job exits (`JobEvent::Finished`); handed out once.
    pub fn take_job_events(&self) -> Option<mpsc::UnboundedReceiver<JobEvent>> {
        lock(&self.job_events).take()
    }

    /// Background jobs still running.
    pub fn running_jobs(&self) -> usize {
        self.jobs.running()
    }

    /// Stops every job (stdin closed, SIGTERM, SIGKILL for its whole group); each running
    /// background job then yields `JobEvent::Finished { exit_code: None }`.
    pub async fn shutdown(&self) {
        self.jobs.shutdown(&self.config).await;
    }

    fn with_summary(&self, what: &str, mut outcome: FileToolOutcome) -> FileToolOutcome {
        let _ = self;
        outcome.summary = cut_chars(what.lines().next().unwrap_or(""), 120);
        outcome
    }

    fn remember_read(&self, path: PathBuf, mtime: SystemTime) {
        lock(&self.reads).insert(path, mtime);
    }

    fn read_mtime(&self, path: &std::path::Path) -> Option<SystemTime> {
        lock(&self.reads).get(path).copied()
    }
}

fn parse<T: DeserializeOwned>(
    name: &str,
    args: serde_json::Value,
) -> Result<T, Box<FileToolOutcome>> {
    serde_json::from_value(args).map_err(|e| {
        Box::new(FileToolOutcome::error(format!(
            "invalid arguments for {name}: {e}"
        )))
    })
}

fn summary(name: &str) -> String {
    name.to_owned()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The first `max` characters of `s`, with `…` when cut.
fn cut_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_owned(),
    }
}

/// `s` cut to at most `max` characters by eliding the middle.
fn elide_middle(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_owned();
    }
    let keep = max / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s.chars().skip(total - keep).collect();
    format!(
        "{head}\n… ({} characters elided) …\n{tail}",
        total - 2 * keep
    )
}

/// The last 20 lines of `s`, at most 2,000 characters.
fn tail_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(20);
    let tail = lines[start..].join("\n");
    let count = tail.chars().count();
    if count <= 2_000 {
        tail
    } else {
        tail.chars().skip(count - 2_000).collect()
    }
}

/// `n` with thousands separators, as the results show counts.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
