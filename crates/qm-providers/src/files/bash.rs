//! `bash`: a shell command that returns within about 3 seconds or becomes background job `J<n>`.

use super::jobs::Job;
use super::{FileToolOutcome, FileTools, FileToolsConfig, JobEvent, elide_middle};
use qm_engine::supervisor::{ProcessOwner, SpawnSpec};
use qm_tools::defs::files::BashArgs;
use qm_types::env::NEVER_PASSED_TO_AGENTS;
use qm_types::ids::JobId;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How long `bash` waits before a command becomes a background job.
const FOREGROUND: Duration = Duration::from_secs(3);
/// The most characters of output a foreground result carries; the middle is elided.
const MAX_CHARS: usize = 30_000;

pub(super) async fn run(tools: &FileTools, args: &BashArgs) -> FileToolOutcome {
    let config = &tools.config;
    if let Err(e) = std::fs::create_dir_all(&config.jobs_dir) {
        return FileToolOutcome::error(format!(
            "couldn't create {}: {e}",
            config.jobs_dir.display()
        ));
    }
    let n = tools.jobs.allocate(&config.jobs_dir);
    let Some(id) = JobId::new(n) else {
        return FileToolOutcome::error("couldn't number the job");
    };
    let log_path = config.jobs_dir.join(format!("{id}.log"));
    let log = match std::fs::File::create(&log_path) {
        Ok(f) => tokio::fs::File::from_std(f),
        Err(e) => {
            return FileToolOutcome::error(format!("couldn't create {}: {e}", log_path.display()));
        }
    };
    let spec = SpawnSpec {
        program: shell(config),
        args: vec!["-c".to_owned(), args.command.clone()],
        cwd: config.cwd.clone(),
        env: config
            .env
            .iter()
            .filter(|(k, _)| !NEVER_PASSED_TO_AGENTS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        owner: ProcessOwner::Job {
            agent: config.agent,
            job: id,
        },
    };
    let child = match config.supervisor.spawn(spec) {
        Ok(child) => child,
        Err(e) => return FileToolOutcome::error(e.to_string()),
    };
    let job = tools.jobs.start(
        id,
        args.command.clone(),
        log_path.clone(),
        log,
        child,
        Arc::clone(&config.clock),
    );
    let mut guard = ForegroundGuard {
        job: Some(Arc::clone(&job)),
        config: config.clone(),
    };

    // A short explicit timeout applies to a foreground command and kills it.
    let short_timeout = args
        .timeout
        .filter(|&ms| !args.run_in_background && ms < 3_000)
        .map(Duration::from_millis);
    if !args.run_in_background {
        let wait = short_timeout.unwrap_or(FOREGROUND);
        let mut done = job.done();
        tokio::select! {
            _ = done.wait_for(|d| *d) => {
                guard.disarm();
                return finished(&job, None);
            }
            () = config.clock.sleep(wait) => {}
        }
        if let Some(timeout) = short_timeout {
            guard.disarm();
            if job.exit().is_none()
                && let Err(e) = job.stop(config).await
            {
                return FileToolOutcome::error(format!("couldn't stop {}: {e}", job.label));
            }
            job.wait_done().await;
            return finished(&job, Some(timeout));
        }
    }
    guard.disarm();
    if !job.to_background() {
        job.wait_done().await;
        return finished(&job, None);
    }
    let mut out = FileToolOutcome::ok(format!(
        "Running as background job {id} (log: {}). You'll be woken with its exit code and last \
         lines when it exits.",
        log_path.display()
    ));
    out.job_started = Some(JobEvent::Started {
        job: job.label.clone(),
        command: job.command.clone(),
        log_path: job.log_path.clone(),
    });
    out
}

/// `$SHELL` when it is zsh or bash, else `/bin/bash`.
fn shell(config: &FileToolsConfig) -> PathBuf {
    config
        .env
        .get("SHELL")
        .filter(|s| s.ends_with("/zsh") || s.ends_with("/bash"))
        .map_or_else(|| PathBuf::from("/bin/bash"), PathBuf::from)
}

/// The result of a command that finished (or was stopped) in the foreground.
fn finished(job: &Job, timed_out: Option<Duration>) -> FileToolOutcome {
    let text = job.log_text();
    let text = text.trim_end_matches('\n');
    let body = if text.is_empty() {
        "(no output)".to_owned()
    } else {
        elide_middle(text, MAX_CHARS)
    };
    if let Some(timeout) = timed_out {
        return FileToolOutcome::error(format!(
            "{body}\n(timed out after {} ms; stopped)",
            timeout.as_millis()
        ));
    }
    match job.exit().and_then(super::jobs::Ended::code) {
        Some(0) => FileToolOutcome::ok(format!("{body}\n(exit code 0)")),
        Some(code) => FileToolOutcome::error(format!("{body}\n(exit code {code})")),
        None => FileToolOutcome::error(format!("{body}\n(killed by a signal)")),
    }
}

/// Stops a foreground command whose `bash` call was cancelled (the future dropped), so nothing
/// keeps running unseen.
struct ForegroundGuard {
    job: Option<Arc<Job>>,
    config: FileToolsConfig,
}

impl ForegroundGuard {
    fn disarm(&mut self) {
        self.job = None;
    }
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        if !job.running_in_foreground() {
            return;
        }
        job.mark_stopped();
        let config = self.config.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = job.stop(&config).await {
                    tracing::warn!(job = %job.label, error = %e, "couldn't stop a cancelled command");
                }
            });
        }
    }
}
