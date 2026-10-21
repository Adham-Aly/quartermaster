//! A project's setup command in a fresh worktree, under the core's supervisor.
//!
//! The core creates the worktree and runs the setup command itself, rather than through
//! `qm_git::run_setup`, so the command's process group is recorded (`owner = setup`), stopped on
//! quit with everything else, and found by the startup leftover check.
//!
//! - `/bin/sh -c <command>` in the worktree, with `env::setup_env`; stdin closed at once.
//! - stdout and stderr appended to `tasks/T-0042/jobs/setup.log`.
//! - Limited to `qm_git::DEFAULT_SETUP_TIMEOUT` on the clock; past it, stopped.
//! - Quitting (or the task being stopped) stops it; the next launch runs it again.

use super::TaskService;
use super::env::setup_env;
use qm_engine::supervisor::{ChildOutput, ExitCause, ProcessOwner, SpawnSpec, StopPolicy, stop};
use qm_types::data_dir::TaskPaths;
use qm_types::ids::{PortBlock, TaskId};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead as _, ReadBuf};
use tokio::sync::watch;

/// How many lines of output a failure quotes.
pub const TAIL_LINES: usize = 20;
/// How long the output readers get to drain after the command exits.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Why the setup command didn't succeed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SetupError {
    #[error("The project's setup command couldn't be started: {0}")]
    Spawn(String),
    #[error("The project's setup command {how} after {after}; the end of its output: {tail}")]
    Failed {
        how: String,
        after: String,
        tail: String,
    },
    #[error(
        "The project's setup command didn't finish within {limit} and was stopped; the end of its output: {tail}"
    )]
    TimedOut { limit: String, tail: String },
    /// The app is quitting or the task is being stopped: not a failure of the task.
    #[error("the setup command was stopped")]
    Cancelled,
}

/// The setup log of a task.
pub(crate) fn log_path(paths: &TaskPaths) -> PathBuf {
    paths.jobs.join("setup.log")
}

/// Runs the setup command in the task's workplace; returns how long it took. `cancel` turns true
/// when the task is being stopped or the app quits.
pub(crate) async fn run(
    svc: &TaskService,
    task: TaskId,
    paths: &TaskPaths,
    cwd: &Path,
    command: &str,
    ports: PortBlock,
    mut cancel: watch::Receiver<bool>,
) -> Result<Duration, SetupError> {
    let deps = svc.deps();
    let clock = svc.clock_arc();
    let spec = SpawnSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_owned(), command.to_owned()],
        cwd: cwd.to_path_buf(),
        env: setup_env(paths, ports, &deps.inherited_env),
        owner: ProcessOwner::Setup { task },
    };
    let started = clock.instant();
    let mut child = deps
        .supervisor
        .spawn(spec)
        .map_err(|e| SetupError::Spawn(e.to_string()))?;
    drop(child.stdin.take());

    let log = open_log(&log_path(paths));
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mut readers = Vec::new();
    for output in [child.stdout.take(), child.stderr.take()]
        .into_iter()
        .flatten()
    {
        readers.push(tokio::spawn(drain(output, log.clone(), Arc::clone(&tail))));
    }

    let mut exit = child.exit.clone();
    let limit = qm_git::DEFAULT_SETUP_TIMEOUT;
    let outcome = tokio::select! {
        cause = exit.wait() => Ok(cause),
        () = clock.sleep(limit) => Err(false),
        _ = cancel.wait_for(|c| *c) => Err(true),
    };
    let cause = match outcome {
        Ok(cause) => cause,
        Err(cancelled) => {
            if let Err(e) = stop(
                &*deps.supervisor,
                &*clock,
                &mut child,
                StopPolicy::default(),
            )
            .await
            {
                tracing::warn!(%task, error = %e, "couldn't stop the setup command");
            }
            let drained = clock.instant() + DRAIN_GRACE;
            for reader in readers {
                tokio::select! {
                    _ = reader => {}
                    () = clock.sleep_until(drained) => {}
                }
            }
            if cancelled {
                return Err(SetupError::Cancelled);
            }
            return Err(SetupError::TimedOut {
                limit: crate::jobs::duration_words(limit),
                tail: tail_text(&tail),
            });
        }
    };
    let drained = clock.instant() + DRAIN_GRACE;
    for reader in readers {
        tokio::select! {
            _ = reader => {}
            () = clock.sleep_until(drained) => {}
        }
    }
    let took = clock.instant().saturating_duration_since(started);
    match cause {
        ExitCause::Exited { code: 0 } => Ok(took),
        ExitCause::Exited { code } => Err(SetupError::Failed {
            how: format!("exited with code {code}"),
            after: crate::jobs::duration_words(took),
            tail: tail_text(&tail),
        }),
        ExitCause::Signaled { signal } => Err(SetupError::Failed {
            how: format!("was killed by signal {signal}"),
            after: crate::jobs::duration_words(took),
            tail: tail_text(&tail),
        }),
    }
}

type Log = Option<Arc<Mutex<std::fs::File>>>;

fn open_log(path: &Path) -> Log {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(Arc::new(Mutex::new(f))),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "couldn't open the setup log");
            None
        }
    }
}

fn tail_text(tail: &Mutex<VecDeque<String>>) -> String {
    let lines = tail.lock().unwrap_or_else(PoisonError::into_inner);
    if lines.is_empty() {
        "(no output)".to_owned()
    } else {
        lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// Copies one pipe into the log and keeps its last lines.
async fn drain(mut output: ChildOutput, log: Log, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut partial = String::new();
    let mut buf = vec![0_u8; 8192];
    loop {
        let n = std::future::poll_fn(|cx| {
            let mut read = ReadBuf::new(&mut buf);
            match std::pin::Pin::new(&mut output).poll_read(cx, &mut read) {
                std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(read.filled().len())),
                std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await;
        let n = match n {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        if let Some(log) = &log {
            let _ = log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_all(chunk);
        }
        partial.push_str(&String::from_utf8_lossy(chunk));
        while let Some(i) = partial.find('\n') {
            let line: String = partial.drain(..=i).collect();
            push_line(&tail, line.trim_end().to_owned());
        }
    }
    if !partial.trim().is_empty() {
        push_line(&tail, partial.trim_end().to_owned());
    }
}

fn push_line(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut lines = tail.lock().unwrap_or_else(PoisonError::into_inner);
    lines.push_back(line);
    while lines.len() > TAIL_LINES {
        lines.pop_front();
    }
}
