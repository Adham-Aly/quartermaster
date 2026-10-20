//! The process supervisor: no process outlives the app.
//!
//! Every agent process (one `claude` per Claude agent) and every shell job starts in its own
//! process group, through a [`Supervisor`]. Children exit when their stdin closes, which happens
//! even if `quartermaster` is killed outright. To stop one, [`stop`] closes its stdin, waits
//! briefly, then sends SIGTERM and finally SIGKILL to the whole group. A process that exits without
//! being asked is a crash, reported through its [`ExitWatch`] and never hung on. Every group is
//! recorded (through a [`ProcessRecorder`] the core backs with the `processes` table), and at
//! startup [`find_leftovers`] reports any recorded group from the last run that is still alive,
//! and [`Supervisor::listeners`] anything still listening on a closed task's ports.
//!
//! A recorded group is recognised by its leader's start time, never by program name: a leader that
//! `exec`s (`zsh -c` becoming `node`) keeps its pid and start time but changes its name, and a
//! group whose leader died can live on in its children.
//!
//! The OS-specific part is the [`Supervisor`] trait: spawning into a new group with an
//! environment built from scratch, signalling a group, and listing a group's living members. The
//! escalation logic in [`stop`] and the leftover check in [`find_leftovers`] are written once on
//! top of it. The real implementation (macOS: `CommandExt::process_group(0)`, `killpg`, libproc
//! for members and start times) is in `supervisor/os.rs`.

use jiff::Timestamp;
use qm_types::clock::Clock;
use qm_types::domain::ProcessOwnerKind;
use qm_types::ids::{AgentId, JobId, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

pub mod os;
pub use os::OsSupervisor;

/// A process group, identified by its group ID (the leader's pid).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessGroup(i32);

impl ProcessGroup {
    pub const fn new(pgid: i32) -> Self {
        Self(pgid)
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

impl fmt::Display for ProcessGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who a process belongs to; recorded with its group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "owner", rename_all = "snake_case")]
pub enum ProcessOwner {
    /// An agent's engine process.
    Agent { agent: AgentId },
    /// A crewmate's background shell job.
    Job { agent: AgentId, job: JobId },
    /// A project's setup command in a fresh worktree.
    Setup { task: TaskId },
    /// Anything else the app supervises.
    App { label: String },
}

impl ProcessOwner {
    pub const fn kind(&self) -> ProcessOwnerKind {
        match self {
            Self::Agent { .. } => ProcessOwnerKind::Agent,
            Self::Job { .. } => ProcessOwnerKind::Job,
            Self::Setup { .. } => ProcessOwnerKind::Setup,
            Self::App { .. } => ProcessOwnerKind::App,
        }
    }
}

/// What to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// The complete environment: nothing is inherited (the environment is built from scratch, so
    /// a stray key in your shell can't reach an agent).
    pub env: BTreeMap<String, String>,
    pub owner: ProcessOwner,
}

/// A started process, as recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    /// Its own new group; its children inherit it.
    pub group: ProcessGroup,
    /// The program's file name, for display.
    pub program: String,
    pub owner: ProcessOwner,
    /// The leader's start time **as the OS reports it**, read right after spawning (macOS:
    /// libproc's process start time), never the app's own clock: the leftover check compares it
    /// with the living leader's start time from the same source.
    pub started_at: Timestamp,
}

/// A living member of a process group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupMember {
    pub pid: u32,
    /// Its program's file name now, after any `exec`: for display only.
    pub program: String,
    /// When it started, as the OS reports it.
    pub started_at: Timestamp,
}

/// A child's pipe to write to.
pub type ChildStdin = Box<dyn AsyncWrite + Send + Unpin>;
/// A child's pipe to read from.
pub type ChildOutput = Box<dyn AsyncRead + Send + Unpin>;

/// A supervised child process. Dropping `stdin` closes it, which is the first stop stage.
pub struct Child {
    pub info: ProcessInfo,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildOutput>,
    pub stderr: Option<ChildOutput>,
    /// Resolves when the group leader exits, for whatever reason.
    pub exit: ExitWatch,
}

impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Child")
            .field("info", &self.info)
            .field("stdin_open", &self.stdin.is_some())
            .field("exit", &self.exit.get())
            .finish_non_exhaustive()
    }
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum ExitCause {
    Exited { code: i32 },
    Signaled { signal: i32 },
}

/// Watches for a group leader's exit. Clone it to watch from several places.
#[derive(Clone, Debug)]
pub struct ExitWatch(watch::Receiver<Option<ExitCause>>);

/// The supervisor's side of an [`ExitWatch`]: reports the exit once.
#[derive(Debug)]
pub struct ExitNotifier(watch::Sender<Option<ExitCause>>);

impl ExitWatch {
    /// A connected notifier and watch.
    pub fn channel() -> (ExitNotifier, ExitWatch) {
        let (tx, rx) = watch::channel(None);
        (ExitNotifier(tx), ExitWatch(rx))
    }

    /// The exit, if it has happened.
    pub fn get(&self) -> Option<ExitCause> {
        *self.0.borrow()
    }

    /// Waits for the exit. If the supervisor drops the notifier without reporting one, the
    /// process is treated as killed, so a waiter is never left hanging.
    pub async fn wait(&mut self) -> ExitCause {
        let seen = match self.0.wait_for(Option::is_some).await {
            Ok(cause) => *cause,
            Err(_) => None,
        };
        seen.or_else(|| self.get())
            .unwrap_or(ExitCause::Signaled { signal: 9 })
    }
}

impl ExitNotifier {
    pub fn exited(self, cause: ExitCause) {
        // No receiver left just means nobody is watching any more.
        let _ = self.0.send(Some(cause));
    }
}

/// A signal sent to a whole process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM.
    Term,
    /// SIGKILL.
    Kill,
}

/// The OS-specific part of supervision.
pub trait Supervisor: Send + Sync + fmt::Debug {
    /// Starts a process in a new process group, with exactly `spec.env` as its environment and
    /// piped stdin, stdout and stderr, and records it.
    fn spawn(&self, spec: SpawnSpec) -> Result<Child, SupervisorError>;

    /// Sends a signal to every process in the group. A group that no longer exists is not an
    /// error.
    fn signal_group(&self, group: ProcessGroup, signal: Signal) -> Result<(), SupervisorError>;

    /// Whether any process in the group is still alive.
    fn group_alive(&self, group: ProcessGroup) -> bool;

    /// The group's living members, the leader (pid == group ID) among them while it lives; empty
    /// once the group is gone.
    fn group_members(&self, group: ProcessGroup) -> Vec<GroupMember>;

    /// Processes listening on any of these TCP ports.
    fn listeners(&self, ports: RangeInclusive<u16>) -> Result<Vec<PortListener>, SupervisorError>;
}

/// Records process groups so the next startup can check for leftovers. The core implements it
/// with the `processes` table; the OS supervisor calls it on every spawn and exit. Calls must not
/// block: queue the write.
pub trait ProcessRecorder: Send + Sync + fmt::Debug {
    fn started(&self, info: &ProcessInfo);
    fn exited(&self, group: ProcessGroup, cause: ExitCause);
}

/// What went wrong supervising a process.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("couldn't start {program}: {message}")]
    Spawn { program: String, message: String },
    #[error("couldn't signal process group {group}: {message}")]
    Signal {
        group: ProcessGroup,
        message: String,
    },
    #[error("couldn't list listening ports: {0}")]
    Listeners(String),
}

/// How long each stop stage waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopPolicy {
    /// After closing stdin, before SIGTERM.
    pub stdin_grace: Duration,
    /// After SIGTERM, before SIGKILL.
    pub term_grace: Duration,
    /// After SIGKILL, before reporting survivors.
    pub kill_grace: Duration,
    /// How often to check whether the group's other members are gone.
    pub poll_every: Duration,
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            stdin_grace: Duration::from_secs(2),
            term_grace: Duration::from_secs(3),
            kill_grace: Duration::from_secs(2),
            poll_every: Duration::from_millis(50),
        }
    }
}

/// How far a requested stop had to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopStage {
    StdinClosed,
    Terminated,
    Killed,
}

/// How a process group ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitReport {
    pub group: ProcessGroup,
    /// The leader's exit, if it was seen.
    pub cause: Option<ExitCause>,
    /// The stage a requested stop reached; `None` means nobody asked it to stop: a crash.
    pub stage: Option<StopStage>,
    /// Members still alive after SIGKILL and its grace period (a process that escaped or can't
    /// die); the next startup's leftover check will report them again.
    pub survivors: bool,
}

impl ExitReport {
    /// A process that exited without being asked to.
    pub fn unrequested(group: ProcessGroup, cause: ExitCause) -> Self {
        Self {
            group,
            cause: Some(cause),
            stage: None,
            survivors: false,
        }
    }
}

/// Stops a child's whole group: closes stdin, waits `stdin_grace`, sends SIGTERM, waits
/// `term_grace`, sends SIGKILL, waits `kill_grace`. Each wait ends early once the leader has exited
/// and the group is empty. A child that had already exited on its own is reported as such without
/// signalling anything.
pub async fn stop(
    sup: &dyn Supervisor,
    clock: &dyn Clock,
    child: &mut Child,
    policy: StopPolicy,
) -> Result<ExitReport, SupervisorError> {
    let group = child.info.group;
    if let Some(cause) = child.exit.get()
        && !sup.group_alive(group)
    {
        drop(child.stdin.take());
        return Ok(ExitReport::unrequested(group, cause));
    }

    drop(child.stdin.take());
    if wait_until_gone(
        sup,
        clock,
        child,
        clock.instant() + policy.stdin_grace,
        policy,
    )
    .await
    {
        return Ok(report(child, StopStage::StdinClosed, false));
    }

    sup.signal_group(group, Signal::Term)?;
    if wait_until_gone(
        sup,
        clock,
        child,
        clock.instant() + policy.term_grace,
        policy,
    )
    .await
    {
        return Ok(report(child, StopStage::Terminated, false));
    }

    sup.signal_group(group, Signal::Kill)?;
    let gone = wait_until_gone(
        sup,
        clock,
        child,
        clock.instant() + policy.kill_grace,
        policy,
    )
    .await;
    Ok(report(child, StopStage::Killed, !gone))
}

fn report(child: &Child, stage: StopStage, survivors: bool) -> ExitReport {
    ExitReport {
        group: child.info.group,
        cause: child.exit.get(),
        stage: Some(stage),
        survivors,
    }
}

/// Waits until the leader has exited and the group is empty, or the deadline passes. Returns
/// whether the group is gone.
async fn wait_until_gone(
    sup: &dyn Supervisor,
    clock: &dyn Clock,
    child: &mut Child,
    deadline: Instant,
    policy: StopPolicy,
) -> bool {
    if child.exit.get().is_none() {
        let mut exit = child.exit.clone();
        tokio::select! {
            _ = exit.wait() => {}
            () = clock.sleep_until(deadline) => return false,
        }
    }
    loop {
        if !sup.group_alive(child.info.group) {
            return true;
        }
        let now = clock.instant();
        if now >= deadline {
            return false;
        }
        clock
            .sleep_until(deadline.min(now + policy.poll_every))
            .await;
    }
}

/// A process group recorded in a previous run (a `processes` row with no `ended_at`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedGroup {
    pub group: ProcessGroup,
    /// The leader's start time as recorded ([`ProcessInfo::started_at`]): what identifies the
    /// group.
    pub started_at: Timestamp,
    /// The program recorded for it, for display only.
    pub program: String,
    pub owner: ProcessOwner,
}

/// A recorded group that is still alive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leftover {
    pub recorded: RecordedGroup,
    /// Its living members, for the report.
    pub members: Vec<GroupMember>,
}

/// The startup check: which recorded groups from the last run are still ours and alive. Reported,
/// never killed automatically.
///
/// A living group is ours if its leader (pid == group ID) is alive with the recorded start time,
/// or if the leader is gone and the group lives on in its children: the OS never hands out a
/// process ID while a group with that ID exists. A living leader with another start time means
/// the ID was reused by something else. Program names are never compared: a leader that `exec`s
/// changes its name, not its pid or start time.
pub fn find_leftovers(sup: &dyn Supervisor, recorded: &[RecordedGroup]) -> Vec<Leftover> {
    recorded
        .iter()
        .filter_map(|r| {
            let members = sup.group_members(r.group);
            is_recorded_group(r, &members).then(|| Leftover {
                recorded: r.clone(),
                members,
            })
        })
        .collect()
}

fn is_recorded_group(recorded: &RecordedGroup, members: &[GroupMember]) -> bool {
    if members.is_empty() {
        return false;
    }
    let leader = u32::try_from(recorded.group.get())
        .ok()
        .and_then(|pid| members.iter().find(|m| m.pid == pid));
    match leader {
        Some(leader) => same_start(leader.started_at, recorded.started_at),
        None => true,
    }
}

/// Start times are compared to the millisecond: the precision `processes.started_at` keeps.
fn same_start(a: Timestamp, b: Timestamp) -> bool {
    a.as_millisecond() == b.as_millisecond()
}

/// A process listening on a TCP port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortListener {
    pub port: u16,
    pub pid: u32,
    pub program: String,
}
