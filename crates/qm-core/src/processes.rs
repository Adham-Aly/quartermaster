//! Recording process groups and the startup leftover check.
//!
//! - [`StoreRecorder`] implements the supervisor's [`ProcessRecorder`] with the `processes`
//!   table. The supervisor calls it on every spawn and exit, from wherever it runs, so it never
//!   blocks: each write is queued, in order, to a small forwarding thread that hands it to the
//!   database thread.
//! - [`startup_leftovers`] runs once at startup, before any agent is started: it checks every
//!   group the last run recorded and never saw exit with [`find_leftovers`] (by the leader's start
//!   time, never by name), lists anything still listening on a closed task's ports, and retires
//!   the recorded groups that are gone. Leftovers are reported, never killed.

use qm_engine::supervisor::{
    ExitCause, Leftover, PortListener, ProcessGroup, ProcessInfo, ProcessOwner, ProcessRecorder,
    RecordedGroup, Supervisor, find_leftovers,
};
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::processes::{
    NewProcess, OpenProcess, ProcessEnd, close_stale, closed_task_port_bases, insert_started,
    mark_exited, open_groups,
};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::ids::{JobId, JobLabel, PortBlock};
use std::fmt;
use std::future::Future;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};

/// A [`ProcessRecorder`] backed by the `processes` table.
pub struct StoreRecorder {
    writes: Mutex<mpsc::Sender<Write>>,
    clock: Arc<dyn Clock>,
    last_error: Arc<Mutex<Option<String>>>,
}

/// One queued write: given the database handle, the future that performs it. A closure, so the
/// wall-clock instant it carries needs no type named here (`jiff` isn't this crate's dependency).
type Write = Box<dyn FnOnce(Db) -> BoxFuture<'static, Result<(), StoreError>> + Send>;

impl fmt::Debug for StoreRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreRecorder").finish_non_exhaustive()
    }
}

impl StoreRecorder {
    /// A recorder writing through `db`; exit times come from `clock`.
    pub fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        let (tx, rx) = mpsc::channel::<Write>();
        let last_error = Arc::new(Mutex::new(None));
        let errors = Arc::clone(&last_error);
        std::thread::Builder::new()
            .name("qm-process-recorder".into())
            .spawn(move || {
                // Ends when the recorder (the only sender) drops.
                for write in rx {
                    let result = block_on(write(db.clone()));
                    if let Err(e) = result {
                        *errors.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(e.to_string());
                    }
                }
            })
            .expect("the process recorder thread could not be spawned");
        Self {
            writes: Mutex::new(tx),
            clock,
            last_error,
        }
    }

    /// The last write that failed, if any, for the startup report and logs.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn queue(&self, write: Write) {
        // The thread lives as long as the recorder; a send can only fail if it panicked.
        let sent = self
            .writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(write);
        if sent.is_err() {
            *self
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some("the process recorder thread stopped".into());
        }
    }
}

impl ProcessRecorder for StoreRecorder {
    fn started(&self, info: &ProcessInfo) {
        let process = new_process(info);
        self.queue(Box::new(move |db| {
            Box::pin(async move { db.call(move |conn| insert_started(conn, &process)).await })
        }));
    }

    fn exited(&self, group: ProcessGroup, cause: ExitCause) {
        let end = match cause {
            ExitCause::Exited { code } => ProcessEnd::Exited { code },
            ExitCause::Signaled { signal } => ProcessEnd::Signaled { signal },
        };
        let pgid = group.get();
        let at = self.clock.now();
        self.queue(Box::new(move |db| {
            Box::pin(async move {
                db.call(move |conn| mark_exited(conn, pgid, end, false, at).map(drop))
                    .await
            })
        }));
    }
}

fn new_process(info: &ProcessInfo) -> NewProcess {
    let (owner, agent, task, label) = match &info.owner {
        ProcessOwner::Agent { agent } => (info.owner.kind(), Some(*agent), None, None),
        ProcessOwner::Job { agent, job } => (
            info.owner.kind(),
            Some(*agent),
            None,
            Some(JobLabel::from(*job).to_string()),
        ),
        ProcessOwner::Setup { task } => (info.owner.kind(), None, Some(*task), None),
        ProcessOwner::App { label } => (info.owner.kind(), None, None, Some(label.clone())),
    };
    NewProcess {
        pgid: info.group.get(),
        pid: info.pid,
        program: info.program.clone(),
        owner,
        agent,
        task,
        label,
        started_at: info.started_at,
    }
}

/// What the startup check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeftoverReport {
    /// Recorded groups from the last run that are still alive, with what runs in them now.
    pub leftovers: Vec<Leftover>,
    /// Anything listening on a closed task's ports.
    pub listeners: Vec<PortListener>,
    /// Port ranges that couldn't be checked, with why, in a plain sentence each.
    pub problems: Vec<String>,
}

/// The startup leftover check. Call it once, before starting any process, so every open row
/// belongs to an earlier run.
pub async fn startup_leftovers(
    db: &Db,
    sup: &dyn Supervisor,
    clock: &dyn Clock,
) -> Result<LeftoverReport, StoreError> {
    let open = db.call(|conn| open_groups(conn)).await?;
    let recorded: Vec<RecordedGroup> = open.iter().map(recorded_group).collect();
    let leftovers = find_leftovers(sup, &recorded);

    let gone: Vec<i32> = recorded
        .iter()
        .filter(|r| !leftovers.iter().any(|l| l.recorded.group == r.group))
        .map(|r| r.group.get())
        .collect();

    let bases = db.call(|conn| closed_task_port_bases(conn)).await?;
    let mut listeners = Vec::new();
    let mut problems = Vec::new();
    for (task, base) in bases {
        match sup.listeners(PortBlock { base }.ports()) {
            Ok(found) => listeners.extend(found),
            Err(e) => problems.push(format!("couldn't check the ports of {task}: {e}")),
        }
    }

    if !gone.is_empty() {
        let at = clock.now();
        db.call(move |conn| close_stale(conn, &gone, at)).await?;
    }

    Ok(LeftoverReport {
        leftovers,
        listeners,
        problems,
    })
}

fn recorded_group(row: &OpenProcess) -> RecordedGroup {
    let app = || ProcessOwner::App {
        label: row.label.clone().unwrap_or_else(|| row.program.clone()),
    };
    let owner = match row.owner {
        qm_types::domain::ProcessOwnerKind::Agent => row
            .agent
            .map_or_else(app, |agent| ProcessOwner::Agent { agent }),
        qm_types::domain::ProcessOwnerKind::Job => {
            match (row.agent, row.label.as_deref().and_then(parse_job)) {
                (Some(agent), Some(job)) => ProcessOwner::Job { agent, job },
                _ => app(),
            }
        }
        qm_types::domain::ProcessOwnerKind::Setup => row
            .task
            .map_or_else(app, |task| ProcessOwner::Setup { task }),
        qm_types::domain::ProcessOwnerKind::App => app(),
    };
    RecordedGroup {
        group: ProcessGroup::new(row.pgid),
        started_at: row.started_at,
        program: row.program.clone(),
        owner,
    }
}

fn parse_job(label: &str) -> Option<JobId> {
    label.parse().ok()
}

/// Runs a future to completion on the current thread, parking between polls. The recorder's
/// thread uses it to wait for each database write in turn without a Tokio runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}
