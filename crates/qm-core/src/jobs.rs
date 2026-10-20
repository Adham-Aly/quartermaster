//! Crewmates' background jobs on both engines.
//!
//! Fed by each crewmate's runner from the engine's events:
//!
//! - `JobStarted`: the `jobs` row (its process row linked, whichever came first) and its event, in
//!   one transaction. A running job is a sign of life for up to two hours.
//! - `JobFinished`: the exit recorded and a `job_exited` wake posted for the crewmate, in one
//!   transaction. The runner then claims it into a running turn and injects it, or the wake
//!   starts a turn once the crewmate is idle (its session resumes if it was suspended).
//!
//! On the Claude engine the job is Claude Code's own background shell, labelled with Claude's
//! task ID, and has no log file of ours.

use crate::inbox::Inbox;
use crate::tasks::{TaskService, cut_chars, new_wake};
use qm_store::StoreError;
use qm_store::jobs as job_store;
use qm_store::tasks as task_store;
use qm_types::data_dir::TaskPaths;
use qm_types::domain::WakeKind;
use qm_types::ids::{AgentId, TaskId, WakeId};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `events.kind` of a started job.
pub const JOB_STARTED: &str = "task.job_started";
/// `events.kind` of a finished job.
pub const JOB_FINISHED: &str = "task.job_finished";
/// How much of a job's last output the wake carries.
pub const TAIL_CHARS: usize = 2_000;

/// Who a [`helpers_running`] refusal is for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HelperWait<'a> {
    /// The crewmate, calling this state-changing tool.
    Crewmate(&'a str),
    /// Its supervisor, settling the task.
    Supervisor,
}

/// Why `task`'s crewmate can't change its task's state yet, or its supervisor settle it: a helper
/// agent of the crewmate is still running. A helper may call the crewmate's tools, so while one
/// runs only the read-only ones work: neither a helper nor the crewmate acts for the task until its
/// helpers are done. `None` when none runs.
pub(crate) fn helpers_running(
    conn: &rusqlite::Connection,
    task: TaskId,
    who: HelperWait<'_>,
) -> Result<Option<String>, StoreError> {
    let running = job_store::running_helpers(conn, task)?;
    let Some(first) = running.first() else {
        return Ok(None);
    };
    let what = if first.command.is_empty() {
        String::new()
    } else {
        format!(" ({})", cut_chars(&first.command, 120, "…"))
    };
    let count = match running.len() {
        1 => "a helper agent".to_owned(),
        n => format!("{n} helper agents"),
    };
    Ok(Some(match who {
        HelperWait::Crewmate(tool) => format!(
            "{count} of yours is still running{what}: wait for your helpers to finish, then call \
             {tool} yourself. You're woken when each one finishes; stop one with TaskStop if you \
             no longer need it"
        ),
        HelperWait::Supervisor => format!(
            "{task}'s crewmate still has {count} running{what}, so its work can't be settled \
             yet; it's woken when the helper finishes"
        ),
    }))
}

/// A started job, as the engine reported it.
#[derive(Debug)]
pub(crate) struct JobStart {
    pub label: String,
    pub command: String,
    pub log_path: Option<PathBuf>,
    /// How long the command had already run when it became a job (`JobStarted::ran_for_ms`).
    pub ran_for_ms: Option<u64>,
}

/// Records a started job.
pub(crate) async fn job_started(
    svc: &TaskService,
    task: TaskId,
    agent: AgentId,
    job: JobStart,
) -> Result<(), StoreError> {
    let JobStart {
        label,
        command,
        log_path,
        ran_for_ms,
    } = job;
    let at = svc.clock().now();
    // A command moved to the background after it started counts from when it started.
    let started = ran_for_ms
        .and_then(|ms| {
            at.checked_sub(jiff::SignedDuration::from_millis(i64::try_from(ms).ok()?))
                .ok()
        })
        .unwrap_or(at);
    svc.write(move |tx, _| {
        job_store::insert_started(
            tx,
            agent,
            task,
            &label,
            &command,
            log_path.as_deref(),
            started,
        )?;
        qm_store::agents::touch(tx, agent, at)?;
        task_store::event(
            tx,
            JOB_STARTED,
            task,
            Some(agent),
            &format!("{task} started {label}: {}", cut_chars(&command, 80, "…")),
            serde_json::json!({ "job": label, "command": command }),
            at,
        )?;
        Ok(())
    })
    .await
}

/// Records a finished job and posts its `job_exited` wake; returns the wake (`None` for a repeat
/// report or a duplicate key).
pub(crate) async fn job_finished(
    svc: &TaskService,
    task: TaskId,
    agent: AgentId,
    label: String,
    exit_code: Option<i32>,
    tail: String,
    claude: bool,
) -> Result<Option<WakeId>, StoreError> {
    let at = svc.clock().now();
    let paths = svc.deps().data_dir.task(task);
    svc.write(move |tx, notify| {
        let Some(job) = job_store::finish(tx, agent, &label, exit_code, at)? else {
            return Ok(None);
        };
        qm_store::agents::touch(tx, agent, at)?;
        let ran: Duration = job
            .started_at
            .duration_until(at)
            .try_into()
            .unwrap_or(Duration::ZERO);
        let text = exit_text(
            &job.command,
            &label,
            exit_code,
            ran,
            job.log_path.as_deref(),
            &paths,
            &tail,
            claude,
        );
        task_store::event(
            tx,
            JOB_FINISHED,
            task,
            Some(agent),
            &format!("{task} {label} {}", exit_words(exit_code)),
            serde_json::json!({ "job": label, "exit_code": exit_code }),
            at,
        )?;
        let wake = new_wake(
            agent,
            WakeKind::JobExited,
            task,
            format!("job:{task}:{label}:{}", job.id),
            &text,
            &format!("{label} {}", exit_words(exit_code)),
            serde_json::json!({ "job": label }),
            at,
        );
        Inbox::post(tx, &wake, notify)
    })
    .await
}

fn exit_words(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("exited with code {code}"),
        None => "was stopped".to_owned(),
    }
}

/// A duration as the wake words it: `48s`, `3m12s`, `1h05m`.
pub fn duration_words(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60)
    }
}

/// The `job_exited` wake's text.
#[allow(clippy::too_many_arguments)]
pub fn exit_text(
    command: &str,
    label: &str,
    exit_code: Option<i32>,
    ran: Duration,
    log_path: Option<&Path>,
    paths: &TaskPaths,
    tail: &str,
    claude: bool,
) -> String {
    let how = match exit_code {
        Some(code) => format!("exited with code {code} after {}", duration_words(ran)),
        None => format!(
            "was stopped or ended without an exit code after {}",
            duration_words(ran)
        ),
    };
    let output = if claude || log_path.is_none() {
        format!("Claude Code's background task {label}")
    } else {
        let path = log_path.unwrap_or(Path::new(""));
        path.strip_prefix(&paths.root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let mut text = format!("`{command}` {how}. Output: {output}");
    let tail = tail.trim_end();
    if !tail.is_empty() {
        let tail: String = {
            let n = tail.chars().count();
            if n > TAIL_CHARS {
                tail.chars().skip(n - TAIL_CHARS).collect()
            } else {
                tail.to_owned()
            }
        };
        text.push_str("\nLast lines:\n");
        text.push_str(&tail);
    }
    text
}
