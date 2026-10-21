//! Every `bash` command is a job: numbered `J<n>` at random per agent, logged to `jobs/J<n>.log`,
//! run in its own process group.
//!
//! A job starts in the foreground: `bash` waits for it. One that outlives that wait becomes a
//! background job, whose exit is reported once as [`JobEvent::Finished`]. A job that finishes in
//! the foreground, or is stopped there, is never reported: its output went back as the tool's
//! result.

use super::{FileToolOutcome, FileToolsConfig, elide_middle, lock, tail_lines};
use crate::files::JobEvent;
use qm_engine::supervisor::{Child, ChildOutput, ExitCause, StopPolicy, StopStage, stop};
use qm_types::clock::Clock;
use qm_types::ids::{JobId, JobLabel, RandomIds, free_number, space};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// The most characters `job_output` returns at once; longer output is elided in the middle.
const MAX_CHARS: usize = 30_000;
/// After the leader exits, how long the output pipes may stay open (a grandchild holding them).
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// The session's jobs.
#[derive(Debug)]
pub(super) struct JobTable {
    events: mpsc::UnboundedSender<JobEvent>,
    /// Where job numbers are drawn from.
    numbers: Mutex<RandomIds>,
    /// The numbers handed out this session.
    handed_out: Mutex<BTreeSet<u32>>,
    jobs: Mutex<BTreeMap<u32, Arc<Job>>>,
}

/// One job.
#[derive(Debug)]
pub(super) struct Job {
    pub(super) label: JobLabel,
    pub(super) command: String,
    pub(super) log_path: PathBuf,
    child: tokio::sync::Mutex<Child>,
    state: Mutex<JobState>,
    done: watch::Sender<bool>,
}

#[derive(Debug)]
struct JobState {
    mode: Mode,
    /// Set once the job has finished.
    exit: Option<Ended>,
    /// Asked to stop (`job_kill`, shutdown, a cancelled or timed-out foreground command).
    stopped: bool,
    /// How far `job_output` has read the log.
    read_offset: u64,
}

/// How a job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ended {
    /// It exited by itself with this code.
    Code(i32),
    /// It was asked to stop (`job_kill`, shutdown, a timeout, a cancelled call).
    Stopped,
    /// It died from a signal nobody here sent.
    Signal,
}

impl Ended {
    pub(super) const fn code(self) -> Option<i32> {
        match self {
            Self::Code(code) => Some(code),
            Self::Stopped | Self::Signal => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Foreground,
    Background,
}

impl JobTable {
    pub(super) fn new(events: mpsc::UnboundedSender<JobEvent>) -> Self {
        Self {
            events,
            numbers: Mutex::new(RandomIds::new()),
            handed_out: Mutex::new(BTreeSet::new()),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    /// A job number at random that nothing here holds: no `J<n>.log` in `jobs_dir` and no number
    /// handed out this session, so labels stay unique per agent across restarts. `None` when
    /// every number is taken.
    pub(super) fn allocate(&self, jobs_dir: &Path) -> Option<u32> {
        let mut taken: BTreeSet<u32> = std::fs::read_dir(jobs_dir).map_or_else(
            |_| BTreeSet::new(),
            |dir| {
                dir.flatten()
                    .filter_map(|e| {
                        let name = e.file_name();
                        let name = name.to_str()?;
                        name.strip_prefix('J')?
                            .strip_suffix(".log")?
                            .parse::<u32>()
                            .ok()
                    })
                    .collect()
            },
        );
        let mut handed_out = lock(&self.handed_out);
        taken.extend(handed_out.iter().copied());
        let n = free_number(&mut *lock(&self.numbers), &space::JOB, &taken)?;
        handed_out.insert(n);
        Some(n)
    }

    /// Registers a freshly spawned job and starts watching it.
    pub(super) fn start(
        self: &Arc<Self>,
        id: JobId,
        command: String,
        log_path: PathBuf,
        log: tokio::fs::File,
        mut child: Child,
        clock: Arc<dyn Clock>,
    ) -> Arc<Job> {
        let log = Arc::new(tokio::sync::Mutex::new(log));
        let pumps: Vec<JoinHandle<()>> = [child.stdout.take(), child.stderr.take()]
            .into_iter()
            .flatten()
            .map(|out| tokio::spawn(pump(out, Arc::clone(&log))))
            .collect();
        let exit = child.exit.clone();
        let (done, _) = watch::channel(false);
        let job = Arc::new(Job {
            label: JobLabel::from(id),
            command,
            log_path,
            child: tokio::sync::Mutex::new(child),
            state: Mutex::new(JobState {
                mode: Mode::Foreground,
                exit: None,
                stopped: false,
                read_offset: 0,
            }),
            done,
        });
        lock(&self.jobs).insert(id.get(), Arc::clone(&job));
        tokio::spawn(monitor(
            Arc::clone(self),
            Arc::clone(&job),
            exit,
            pumps,
            clock,
        ));
        job
    }

    fn get(&self, id: JobId) -> Option<Arc<Job>> {
        lock(&self.jobs).get(&id.get()).cloned()
    }

    /// Background jobs still running.
    pub(super) fn running(&self) -> usize {
        lock(&self.jobs)
            .values()
            .filter(|j| {
                let s = lock(&j.state);
                s.mode == Mode::Background && s.exit.is_none()
            })
            .count()
    }

    /// `job_output`: the output since the last call for this job, then its state.
    pub(super) fn output(&self, id: JobId, filter: Option<&str>) -> FileToolOutcome {
        let Some(job) = self.get(id) else {
            return FileToolOutcome::error(format!("there is no job {id} in this session"));
        };
        let filter = match filter.map(regex::Regex::new).transpose() {
            Ok(f) => f,
            Err(e) => return FileToolOutcome::error(format!("invalid filter: {e}")),
        };
        let offset = lock(&job.state).read_offset;
        let (text, read) = match read_from(&job.log_path, offset) {
            Ok(r) => r,
            Err(e) => {
                return FileToolOutcome::error(format!(
                    "couldn't read {}: {e}",
                    job.log_path.display()
                ));
            }
        };
        let status = {
            let mut s = lock(&job.state);
            s.read_offset = offset + read;
            status_line(&job.label, s.exit)
        };
        let body = match &filter {
            Some(re) => text
                .lines()
                .filter(|l| re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n"),
            None => text.trim_end_matches('\n').to_owned(),
        };
        let body = if body.is_empty() {
            "(no new output)".to_owned()
        } else {
            elide_middle(&body, MAX_CHARS)
        };
        FileToolOutcome::ok(format!("{body}\n{status}"))
    }

    /// `job_kill`: stops the job's whole group.
    pub(super) async fn kill(&self, config: &FileToolsConfig, id: JobId) -> FileToolOutcome {
        let Some(job) = self.get(id) else {
            return FileToolOutcome::error(format!("there is no job {id} in this session"));
        };
        {
            let s = lock(&job.state);
            if s.exit.is_some() {
                return FileToolOutcome::ok(status_line(&job.label, s.exit));
            }
        }
        match job.stop(config).await {
            Ok(stage) => FileToolOutcome::ok(format!(
                "{} stopped: {}",
                job.label,
                match stage {
                    Some(StopStage::StdinClosed) => "it exited when its input closed",
                    Some(StopStage::Terminated) => "it exited on SIGTERM",
                    Some(StopStage::Killed) => "it was killed with SIGKILL",
                    None => "it had already exited",
                }
            )),
            Err(e) => FileToolOutcome::error(format!("couldn't stop {}: {e}", job.label)),
        }
    }

    /// Stops every job still running and waits until each has finished (and, for background
    /// jobs, been reported).
    pub(super) async fn shutdown(&self, config: &FileToolsConfig) {
        let jobs: Vec<Arc<Job>> = lock(&self.jobs).values().cloned().collect();
        for job in jobs {
            if lock(&job.state).exit.is_none()
                && let Err(e) = job.stop(config).await
            {
                tracing::warn!(job = %job.label, error = %e, "couldn't stop a job at shutdown");
            }
        }
    }
}

impl Job {
    /// Marks the job stopped, runs the stop ladder on its group, and waits until it has
    /// finished.
    pub(super) async fn stop(
        &self,
        config: &FileToolsConfig,
    ) -> Result<Option<StopStage>, qm_engine::supervisor::SupervisorError> {
        lock(&self.state).stopped = true;
        let report = {
            let mut child = self.child.lock().await;
            stop(
                config.supervisor.as_ref(),
                config.clock.as_ref(),
                &mut child,
                StopPolicy::default(),
            )
            .await?
        };
        self.wait_done().await;
        Ok(report.stage)
    }

    /// Waits until the job has finished and been reported.
    pub(super) async fn wait_done(&self) {
        let mut done = self.done.subscribe();
        let _ = done.wait_for(|d| *d).await;
    }

    /// A receiver that turns true once the job has finished.
    pub(super) fn done(&self) -> watch::Receiver<bool> {
        self.done.subscribe()
    }

    /// Moves a running job to the background. `false` if it has already finished.
    pub(super) fn to_background(&self) -> bool {
        let mut s = lock(&self.state);
        if s.exit.is_some() {
            false
        } else {
            s.mode = Mode::Background;
            true
        }
    }

    /// Whether the job is still a running foreground job.
    pub(super) fn running_in_foreground(&self) -> bool {
        let s = lock(&self.state);
        s.mode == Mode::Foreground && s.exit.is_none()
    }

    /// How the job ended, once it has.
    pub(super) fn exit(&self) -> Option<Ended> {
        lock(&self.state).exit
    }

    pub(super) fn mark_stopped(&self) {
        lock(&self.state).stopped = true;
    }

    /// The whole log so far.
    pub(super) fn log_text(&self) -> String {
        read_from(&self.log_path, 0)
            .map(|(text, _)| text)
            .unwrap_or_default()
    }
}

/// Copies one output pipe into the shared log.
async fn pump(mut out: ChildOutput, log: Arc<tokio::sync::Mutex<tokio::fs::File>>) {
    let mut buf = vec![0u8; 8_192];
    loop {
        match out.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut file = log.lock().await;
                if file.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                let _ = file.flush().await;
            }
        }
    }
}

/// Waits for the job to end, records it, and reports a background job's exit.
async fn monitor(
    table: Arc<JobTable>,
    job: Arc<Job>,
    mut exit: qm_engine::supervisor::ExitWatch,
    pumps: Vec<JoinHandle<()>>,
    clock: Arc<dyn Clock>,
) {
    let cause = exit.wait().await;
    let aborts: Vec<_> = pumps.iter().map(JoinHandle::abort_handle).collect();
    tokio::select! {
        () = async { for p in pumps { let _ = p.await; } } => {}
        () = clock.sleep(PIPE_GRACE) => {
            for a in aborts { a.abort(); }
        }
    }
    let (report, code) = {
        let mut s = lock(&job.state);
        let ended = match cause {
            _ if s.stopped => Ended::Stopped,
            ExitCause::Exited { code } => Ended::Code(code),
            ExitCause::Signaled { .. } => Ended::Signal,
        };
        s.exit = Some(ended);
        (s.mode == Mode::Background, ended.code())
    };
    if report {
        let tail = tail_lines(&job.log_text());
        let _ = table.events.send(JobEvent::Finished {
            job: job.label.clone(),
            exit_code: code,
            tail,
        });
    }
    // `send_replace`, not `send`: with nobody subscribed yet, `send` drops the value, and a
    // later `wait_done` would wait for ever.
    job.done.send_replace(true);
}

/// The log's text from `offset`, and how many bytes were read.
fn read_from(path: &Path, offset: u64) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let read = bytes.len() as u64;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), read))
}

fn status_line(label: &JobLabel, exit: Option<Ended>) -> String {
    match exit {
        None => format!("{label} is running"),
        Some(Ended::Code(code)) => format!("{label} exited with code {code}"),
        Some(Ended::Stopped) => format!("{label} was stopped"),
        Some(Ended::Signal) => format!("{label} was killed by a signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::JobTable;
    use std::path::{Path, PathBuf};
    use tokio::sync::mpsc;

    /// A folder under the system temp directory, removed on drop.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("qm-jobs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch folder");
            Self(path)
        }

        /// Writes a `J<n>.log` for each of `numbers`.
        fn logs(&self, numbers: impl Iterator<Item = u32>) {
            for n in numbers {
                std::fs::write(self.0.join(format!("J{n}.log")), "").expect("log");
            }
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn table() -> JobTable {
        JobTable::new(mpsc::unbounded_channel().0)
    }

    #[test]
    fn the_only_free_number_is_the_one_handed_out_and_then_none_is_left() {
        let folder = Folder::new("one-free");
        folder.logs((1..=999).filter(|n| *n != 42));
        let jobs = table();
        assert_eq!(jobs.allocate(&folder.0), Some(42));
        // Handed out this session, so it isn't free again even before its log exists.
        assert_eq!(jobs.allocate(&folder.0), None);
    }

    #[test]
    fn a_number_is_drawn_from_the_job_space_in_a_fresh_folder() {
        let folder = Folder::new("fresh");
        let jobs = table();
        let n = jobs.allocate(Path::new(&folder.0)).expect("a number");
        assert!((1..=999).contains(&n));
    }
}
