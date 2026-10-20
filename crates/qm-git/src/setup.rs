//! A project's setup command, run in a fresh worktree.
//!
//! Each project may name one setup command (`projects.setup_command`). The app runs it in the
//! new worktree before the crewmate starts; installing dependencies stays the crewmate's job.
//! It runs through `/bin/sh -c`, with the app's environment minus every inherited `GIT_*`
//! variable (so any git it runs works on the worktree), plus whatever the core adds (for example
//! `QM_EVIDENCE_DIR` and `QM_PORT_BASE`). Its standard input is closed, it runs in its own
//! process group, and past its time limit everything it started is stopped.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::GitError;
use crate::git::scrub_git_env;
use crate::process::{self, OutputMode};

/// The time limit a setup command gets unless the core sets another.
pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_mins(30);

/// The most output kept from a setup command (its end).
const SETUP_OUTPUT_CAP: usize = 1024 * 1024;

/// How much of the output's end a failure reports.
const SETUP_ERROR_TAIL: usize = 8 * 1024;

/// A project's setup command, ready to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupCommand {
    /// The shell command line, exactly as the project records it.
    pub command: String,
    /// Extra variables for it, set after the inherited `GIT_*` ones were removed.
    pub env: Vec<(String, String)>,
    /// Its time limit.
    pub timeout: Duration,
}

impl SetupCommand {
    /// A setup command with no extra variables and [`DEFAULT_SETUP_TIMEOUT`].
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            env: Vec::new(),
            timeout: DEFAULT_SETUP_TIMEOUT,
        }
    }
}

/// A setup command that finished successfully.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupOutcome {
    /// What it printed (stdout and stderr together, in order), at most its last megabyte.
    pub output: String,
    /// How long it took.
    pub duration: Duration,
}

/// Runs `setup` in `worktree` and waits for it. A non-zero exit is
/// [`GitError::SetupFailed`] and a time limit [`GitError::SetupTimedOut`], each with the end of
/// what it printed.
pub fn run_setup(worktree: &Path, setup: &SetupCommand) -> Result<SetupOutcome, GitError> {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(&setup.command).current_dir(worktree);
    scrub_git_env(&mut command);
    for (key, value) in &setup.env {
        command.env(key, value);
    }
    tracing::info!(worktree = %worktree.display(), command = %setup.command, "running setup command");
    let started = Instant::now();
    let captured = process::run(
        command,
        setup.timeout,
        OutputMode::Combined,
        SETUP_OUTPUT_CAP,
    )
    .map_err(|source| GitError::SetupSpawn {
        command: setup.command.clone(),
        source,
    })?;
    let output = String::from_utf8_lossy(&captured.stdout).into_owned();
    if captured.timed_out {
        return Err(GitError::SetupTimedOut {
            command: setup.command.clone(),
            after: setup.timeout,
            output: tail(&output, SETUP_ERROR_TAIL),
        });
    }
    if !captured.success() {
        return Err(GitError::SetupFailed {
            command: setup.command.clone(),
            code: captured.code,
            output: tail(&output, SETUP_ERROR_TAIL),
        });
    }
    Ok(SetupOutcome {
        output,
        duration: started.elapsed(),
    })
}

/// The last `max` bytes of `text`, cut at a character boundary.
fn tail(text: &str, max: usize) -> String {
    let text = text.trim_end();
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}
