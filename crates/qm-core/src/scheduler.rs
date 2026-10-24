//! Keyed one-shot jobs at deadlines.
//!
//! One Tokio task. It holds nothing durable: every owner re-registers its deadlines from the
//! database at startup, and every job is idempotent. A job registered under a key replaces the
//! one already there.
//!
//! - [`Scheduler::after`] measures on `clock.instant()`, app-running time.
//! - [`Scheduler::at_time`] is for times of day: it sleeps at most [`AT_TIME_RECHECK`] at a time
//!   and re-checks `clock.now()` on waking, so a Mac that slept fires it as soon as it wakes. A
//!   time already past fires at once.
//!
//! Its users: the re-route tick for queued tasks (`tasks:reroute`), account resets
//! (`account_reset:C-0042`) and due schedules (`at_time`). Watchdog ladders live in
//! their runners, not here.

use jiff::Timestamp;
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A job: runs once, on its own Tokio task.
pub type Job = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// The longest an `at_time` job sleeps before it re-checks the wall clock.
pub const AT_TIME_RECHECK: Duration = Duration::from_secs(60);

enum When {
    /// After this long, from when the scheduler's task receives the request.
    Delay(Duration),
    Instant(Instant),
    Time(Timestamp),
}

enum Request {
    Add { key: String, when: When, job: Job },
    Cancel { key: String },
}

/// A handle on the scheduler's task; cheap to clone. The task ends when every handle is gone.
#[derive(Clone)]
pub struct Scheduler {
    tx: mpsc::UnboundedSender<Request>,
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

impl Scheduler {
    /// Starts the scheduler's task. Needs a Tokio runtime.
    pub fn spawn(clock: Arc<dyn Clock>) -> (Scheduler, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run(clock, rx));
        (Scheduler { tx }, handle)
    }

    /// Runs `job` after `delay` of app-running time, replacing any job with the same key.
    pub fn after(&self, key: impl Into<String>, delay: Duration, job: Job) {
        self.send(Request::Add {
            key: key.into(),
            when: When::Delay(delay),
            job,
        });
    }

    /// Runs `job` once `clock.now()` reaches `at`, replacing any job with the same key.
    pub fn at_time(&self, key: impl Into<String>, at: Timestamp, job: Job) {
        self.send(Request::Add {
            key: key.into(),
            when: When::Time(at),
            job,
        });
    }

    /// Drops the job with this key, if any.
    pub fn cancel(&self, key: &str) {
        self.send(Request::Cancel {
            key: key.to_owned(),
        });
    }

    fn send(&self, request: Request) {
        if self.tx.send(request).is_err() {
            tracing::warn!("the scheduler has stopped; a job was dropped");
        }
    }
}

async fn run(clock: Arc<dyn Clock>, mut rx: mpsc::UnboundedReceiver<Request>) {
    let mut jobs: HashMap<String, (When, Job)> = HashMap::new();
    loop {
        fire_due(&*clock, &mut jobs);
        let next = next_wake(&*clock, &jobs);
        let request = match next {
            Some(deadline) => {
                tokio::select! {
                    request = rx.recv() => request,
                    () = clock.sleep_until(deadline) => continue,
                }
            }
            None => rx.recv().await,
        };
        let Some(request) = request else {
            return;
        };
        match request {
            Request::Add { key, when, job } => {
                // A delay is measured on the scheduler's clock from now.
                let when = match when {
                    When::Delay(delay) => When::Instant(clock.instant() + delay),
                    other => other,
                };
                jobs.insert(key, (when, job));
            }
            Request::Cancel { key } => {
                jobs.remove(&key);
            }
        }
    }
}

fn due(clock: &dyn Clock, when: &When) -> bool {
    match when {
        When::Delay(_) => true,
        When::Instant(at) => clock.instant() >= *at,
        When::Time(at) => clock.now() >= *at,
    }
}

fn fire_due(clock: &dyn Clock, jobs: &mut HashMap<String, (When, Job)>) {
    let keys: Vec<String> = jobs
        .iter()
        .filter(|(_, (when, _))| due(clock, when))
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        if let Some((_, job)) = jobs.remove(&key) {
            tokio::spawn(job());
        }
    }
}

fn next_wake(clock: &dyn Clock, jobs: &HashMap<String, (When, Job)>) -> Option<Instant> {
    let now = clock.instant();
    let wall = clock.now();
    jobs.values()
        .map(|(when, _)| match when {
            When::Delay(_) => now,
            When::Instant(at) => *at,
            When::Time(at) => {
                let left = wall
                    .duration_until(*at)
                    .try_into()
                    .unwrap_or(Duration::ZERO);
                now + left.min(AT_TIME_RECHECK)
            }
        })
        .min()
}
