//! Tasks and their crewmates.
//!
//! [`TaskService`] is the one way in: the tool host delegates every task and crewmate tool to it,
//! the API sends it your interrupts and the quit, settling calls its side exits and hooks. It keeps
//! one runner per open task that has a crewmate ([`runner`]), each registered with the [`Inbox`] as
//! its crewmate's wake consumer.
//!
//! - [`lifecycle`]: the task lifecycle table, pure.
//! - [`start`]: `start_crewmate` (validate, settle the base, check delivery, record) and phase C
//!   (worktree, setup, skills, brief, session), with [`base`], [`delivery_probe`], [`ports`],
//!   [`mod@env`], [`setup`] and [`session`].
//! - [`runner`] and [`input`]: one turn at a time, wakes batched, job exits injected.
//! - [`report`], [`waits`]: the crewmate's tools; [`close`]: every exit, one retirement;
//!   [`lookup`]: `task_status` and `find_tasks`; [`startup`]: reconcile, resume and quit.
//!
//! Every write that causes a wake posts it in the same transaction (`TaskService::write`) and
//! signals after the commit; every write also signals the UI so its snapshot is rebuilt.

pub mod base;
pub mod close;
pub mod delivered;
pub mod delivery_probe;
pub mod env;
pub mod input;
pub mod lifecycle;
pub mod lookup;
pub mod plan;
pub mod ports;
pub mod relabel;
pub mod report;
pub mod runner;
pub mod session;
pub mod setup;
pub mod start;
pub mod startup;
#[cfg(test)]
mod unlink_tests;
pub mod waits;

use crate::api::{ApiPart, Handled};
use crate::inbox::{Inbox, Notify, WakeSink};
use crate::scheduler::Scheduler;
use crate::setup_check::SetupFacts;
use crate::turn_io::ChatLog;
use crate::watchdogs::DecisionChoices;
use delivery_probe::DeliveryProbe;
use jiff::Timestamp;
use qm_auth::LoginHealth;
use qm_engine::Engine;
use qm_engine::supervisor::Supervisor;
use qm_git::Git;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::wakes::NewWake;
use qm_tools::defs::{
    Ack, DeliveryCheck, FileFollowupArgs, FindTasksArgs, FoundTasks, Linked, ReportArgs,
    TaskDetail, TaskStarted, TaskUpdated, Unlinked,
};
use qm_tools::handler::{Caller, DeclaredWait, Dispatcher, StartRequest, ToolFailure, ToolHost};
use qm_types::BoxFuture;
use qm_types::api::{AgentRef, Command, Snapshot};
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::data_dir::DataDir;
use qm_types::domain::{EngineKind, Role, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, MessageId, TaskId};
use report::ReportGate;
use runner::{CrewActivity, CrewInput, RunnerState, StopReport};
use rusqlite::Transaction;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

pub use qm_store::projects::ProjectRow;

/// How often the UI is refreshed while any crewmate is working, so running minutes move.
pub const UI_REFRESH: Duration = Duration::from_secs(1);
/// How long quitting waits for every runner, in all.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
/// The re-route tick for queued tasks.
pub const REROUTE_EVERY: Duration = Duration::from_secs(60);
/// The scheduler key of the re-route tick.
pub const REROUTE_KEY: &str = "tasks:reroute";

pub(crate) use qm_store::projects::{
    active as active_projects, active_by_name, load as load_project,
};

/// What the task service is built from.
pub struct TaskDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub data_dir: DataDir,
    /// Your home folder, where your own `.agents/` lives.
    pub home: PathBuf,
    pub engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    /// The supervisor setup commands run under, and that finds listeners on task ports.
    pub supervisor: Arc<dyn Supervisor>,
    pub inbox: Inbox,
    pub scheduler: Scheduler,
    pub config: watch::Receiver<Config>,
    /// The app's record of which logins work: routing leaves broken accounts out.
    pub health: Arc<LoginHealth>,
    pub git: Git,
    /// The Bridge's chat: app notes, and the change signal that rebuilds the UI's snapshot.
    pub chat: Arc<dyn ChatLog>,
    /// The Quartermaster's agent row.
    pub seat_agent: AgentId,
    pub probe: Arc<dyn DeliveryProbe>,
    pub gate: Arc<dyn ReportGate>,
    pub choices: Arc<dyn DecisionChoices>,
    /// Verification's hook in the start transaction.
    pub start_hook: Arc<dyn start::StartHook>,
    /// The app's own environment, for the basics a setup command gets.
    pub inherited_env: BTreeMap<String, String>,
    /// `Dispatcher::builtin()` in the app.
    pub dispatcher: Dispatcher,
    /// Passive context readings for the UI; its tap goes on every session this opens.
    pub monitor: Arc<crate::context_monitor::ContextMonitor>,
}

impl fmt::Debug for TaskDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskDeps")
            .field("seat_agent", &self.seat_agent)
            .finish_non_exhaustive()
    }
}

/// What the binary hands the task service; the rest takes its defaults ([`TaskService::wire`]).
pub struct TaskWiring {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub data_dir: DataDir,
    pub home: PathBuf,
    pub engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    pub supervisor: Arc<dyn Supervisor>,
    pub inbox: Inbox,
    pub scheduler: Scheduler,
    pub config: watch::Receiver<Config>,
    pub health: Arc<LoginHealth>,
    pub chat: Arc<dyn ChatLog>,
    pub seat_agent: AgentId,
    /// The setup check's facts: the delivery probe reads the GitHub CLI's login there.
    pub setup: watch::Receiver<SetupFacts>,
    /// Delivery's probe with GitHub's API; `None` keeps the local probe.
    pub probe: Option<Arc<dyn DeliveryProbe>>,
    /// Passive context readings for the UI.
    pub monitor: Arc<crate::context_monitor::ContextMonitor>,
}

impl fmt::Debug for TaskWiring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskWiring")
            .field("seat_agent", &self.seat_agent)
            .finish_non_exhaustive()
    }
}

/// Verification's hooks into the task service: the report gate, the start hook and the stuck
/// notice's choices. The defaults pass every report, do nothing at start and give the plain stuck
/// choices.
#[derive(Clone, Debug)]
pub struct TaskHooks {
    pub gate: Arc<dyn ReportGate>,
    pub start_hook: Arc<dyn start::StartHook>,
    pub choices: Arc<dyn DecisionChoices>,
}

impl Default for TaskHooks {
    fn default() -> Self {
        Self {
            gate: Arc::new(report::OpenGate),
            start_hook: Arc::new(start::NoStartHook),
            choices: Arc::new(crate::watchdogs::PlainStuckChoices),
        }
    }
}

/// One live runner.
struct RunnerEntry {
    agent: AgentId,
    handle: CrewHandle,
    state: Arc<RunnerState>,
    join: Option<JoinHandle<()>>,
}

pub(crate) struct Inner {
    pub(crate) deps: TaskDeps,
    runners: Mutex<HashMap<TaskId, RunnerEntry>>,
    host: OnceLock<Weak<dyn ToolHost>>,
    shutting_down: watch::Sender<bool>,
    retiring: Mutex<HashSet<TaskId>>,
    /// The session recorder, set once by the binary.
    sink: OnceLock<Arc<dyn runner::SessionSink>>,
    /// Delivery's hook on every retirement: where landed work went, and side exits.
    pub(crate) close_facts: OnceLock<Arc<dyn close::CloseFacts>>,
    /// Delivery's watch for what happens on GitHub outside the app.
    outside: OnceLock<Arc<dyn OutsideWatch>>,
    /// The plan pages' endpoint port; 0 while it isn't listening.
    plan_port: AtomicU16,
    deferred_pulse_started: AtomicBool,
}

/// Held while a task is being retired; a second retirement of the same task waits its turn by
/// being refused.
pub(crate) struct RetireGuard {
    inner: Arc<Inner>,
    task: TaskId,
}

impl Drop for RetireGuard {
    fn drop(&mut self) {
        self.inner
            .retiring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.task);
    }
}

/// The tasks and their crewmates. Cheap to clone.
#[derive(Clone)]
pub struct TaskService {
    pub(crate) inner: Arc<Inner>,
}

impl fmt::Debug for TaskService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.runners().len();
        f.debug_struct("TaskService")
            .field("runners", &n)
            .finish_non_exhaustive()
    }
}

/// A runner's input channel: kept by the service per task, and its crewmate's wake consumer.
#[derive(Clone, Debug)]
pub struct CrewHandle {
    tx: mpsc::UnboundedSender<CrewInput>,
}

impl CrewHandle {
    pub(crate) fn send(&self, input: CrewInput) -> bool {
        self.tx.send(input).is_ok()
    }
}

impl WakeSink for CrewHandle {
    fn wake(&self) {
        let _ = self.tx.send(CrewInput::Wake);
    }
}

/// A store error as a tool failure: the app's failure, not a rule.
pub(crate) fn failed(e: &impl fmt::Display) -> ToolFailure {
    ToolFailure::Failed(e.to_string())
}

/// A wake with the payload convention every producer follows (`text`, `short`, and extras).
#[allow(clippy::too_many_arguments)]
pub(crate) fn new_wake(
    target: AgentId,
    kind: WakeKind,
    task: TaskId,
    dedupe_key: String,
    text: &str,
    short: &str,
    extra: serde_json::Value,
    at: Timestamp,
) -> NewWake {
    let mut payload = serde_json::json!({ "text": text, "short": short });
    if let (Some(map), serde_json::Value::Object(extra)) = (payload.as_object_mut(), extra) {
        map.extend(extra);
    }
    NewWake {
        target,
        kind,
        payload,
        dedupe_key,
        task: Some(task),
        at,
    }
}

/// Cuts `text` to `max` characters, ending with `suffix` when cut.
pub(crate) fn cut_chars(text: &str, max: usize, suffix: &str) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(max).collect();
        out.push_str(suffix);
        out
    }
}

/// Delivery's hook on a task's start and report (`delivery::outside`): both run in the background
/// and never hold up the task.
pub trait OutsideWatch: Send + Sync + fmt::Debug {
    /// The task's folders exist: keep the repository's tags as they are now.
    fn started(&self, task: TaskId) -> BoxFuture<'_, ()>;
    /// The crewmate reported: read its pull request and the tags again.
    fn reported(&self, task: TaskId) -> BoxFuture<'_, ()>;
}

impl TaskService {
    /// Starts exactly one deferred-start pulse, after startup recovery has completed.
    /// Weak ownership prevents the pulse retaining the service; shutdown interrupts its sleep.
    pub fn start_deferred_starts(&self) {
        if self
            .inner
            .deferred_pulse_started
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let clock = self.clock_arc();
        let mut shutdown = self.inner.shutting_down.subscribe();
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    break;
                }
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let svc = TaskService { inner };
                if let Err(error) = crate::deferred_starts::pulse(&svc).await {
                    tracing::warn!(%error, "deferred starts pulse failed; pending intent will be retried");
                }
                drop(svc);
                tokio::select! {
                    () = clock.sleep(Duration::from_secs(2)) => {},
                    _ = shutdown.changed() => break,
                }
            }
        });
    }

    /// Sets delivery's outside watch (once; a second call is ignored).
    pub fn set_outside_watch(&self, hook: Arc<dyn OutsideWatch>) {
        if self.inner.outside.set(hook).is_err() {
            tracing::warn!("the outside watch was set twice; the first one stays");
        }
    }

    /// Runs the outside watch's `f` for `task` in the background, if it is set.
    pub(crate) fn tell_outside(&self, task: TaskId, reported: bool) {
        let Some(hook) = self.inner.outside.get().cloned() else {
            return;
        };
        tokio::spawn(async move {
            if reported {
                hook.reported(task).await;
            } else {
                hook.started(task).await;
            }
        });
    }
}

impl TaskService {
    /// The service; starts the UI refresh tick. Needs a Tokio runtime.
    pub fn new(deps: TaskDeps) -> Self {
        let (shutting_down, _) = watch::channel(false);
        let svc = Self {
            inner: Arc::new(Inner {
                deps,
                runners: Mutex::new(HashMap::new()),
                host: OnceLock::new(),
                shutting_down,
                retiring: Mutex::new(HashSet::new()),
                sink: OnceLock::new(),
                close_facts: OnceLock::new(),
                outside: OnceLock::new(),
                plan_port: AtomicU16::new(0),
                deferred_pulse_started: AtomicBool::new(false),
            }),
        };
        svc.spawn_refresh();
        svc
    }

    /// The service with the app's defaults: your `git`, the local
    /// delivery probe, the open report gate and the plain stuck choices (the binary replaces the
    /// last three through [`TaskDeps`]).
    pub fn wire(w: TaskWiring) -> Self {
        Self::wire_with(w, TaskHooks::default())
    }

    /// [`Self::wire`] with verification's hooks.
    pub fn wire_with(w: TaskWiring, hooks: TaskHooks) -> Self {
        let git = Git::new();
        Self::new(TaskDeps {
            db: w.db,
            clock: w.clock,
            data_dir: w.data_dir,
            home: w.home,
            engines: w.engines,
            supervisor: w.supervisor,
            inbox: w.inbox,
            scheduler: w.scheduler,
            config: w.config,
            health: w.health,
            git: git.clone(),
            chat: w.chat,
            seat_agent: w.seat_agent,
            probe: w
                .probe
                .unwrap_or_else(|| Arc::new(delivery_probe::LocalDeliveryProbe::new(git, w.setup))),
            gate: hooks.gate,
            choices: hooks.choices,
            start_hook: hooks.start_hook,
            inherited_env: std::env::vars().collect(),
            dispatcher: Dispatcher::builtin(),
            monitor: w.monitor,
        })
    }

    /// The tool host crewmates' calls are dispatched to (the core's, which holds this service; a
    /// weak link, so neither keeps the other alive).
    pub fn set_tool_host(&self, host: Weak<dyn ToolHost>) {
        if self.inner.host.set(host).is_err() {
            tracing::warn!("the task service's tool host was set twice; the first one stays");
        }
    }

    pub(crate) fn host(&self) -> Option<Arc<dyn ToolHost>> {
        self.inner.host.get().and_then(Weak::upgrade)
    }

    pub(crate) fn deps(&self) -> &TaskDeps {
        &self.inner.deps
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.deps.db
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        &*self.inner.deps.clock
    }

    pub(crate) fn clock_arc(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.deps.clock)
    }

    /// The port plan pages send their replies to; `None` while the endpoint isn't listening.
    pub(crate) fn plan_port(&self) -> Option<u16> {
        match self.inner.plan_port.load(Ordering::Acquire) {
            0 => None,
            port => Some(port),
        }
    }

    /// Notes the plan pages' endpoint port (`None` once it stops).
    pub(crate) fn set_plan_port(&self, port: Option<u16>) {
        self.inner
            .plan_port
            .store(port.unwrap_or(0), Ordering::Release);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        *self.inner.shutting_down.borrow()
    }

    fn runners(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, RunnerEntry>> {
        self.inner
            .runners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks `task` as being retired; `None` if it already is.
    pub(crate) fn begin_retiring(&self, task: TaskId) -> Option<RetireGuard> {
        let fresh = self
            .inner
            .retiring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task);
        fresh.then(|| RetireGuard {
            inner: Arc::clone(&self.inner),
            task,
        })
    }

    /// Signals the UI that something it shows changed.
    pub(crate) fn changed(&self) {
        self.inner.deps.chat.changed();
    }

    /// Runs `f` in one transaction, then signals every wake target it posted and the UI.
    pub(crate) async fn write<R, F>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&Transaction<'_>, &mut Notify) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        let (r, notify) = self
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                let mut notify = Notify::new();
                let r = f(&tx, &mut notify)?;
                tx.commit()?;
                Ok((r, notify))
            })
            .await?;
        self.inner.deps.inbox.notify(&notify);
        self.changed();
        Ok(r)
    }

    /// Shows a line from the app in the Bridge.
    pub(crate) async fn app_note(&self, text: &str) {
        let agent = self.inner.deps.seat_agent;
        if let Err(e) = self.inner.deps.chat.app_note(agent, None, text).await {
            tracing::warn!(error = %e, "couldn't show an app note");
        }
    }

    /// The supervisor's name as markers carry it: `quartermaster` or `SM-02`.
    pub(crate) async fn supervisor_label(&self, supervisor: AgentId) -> String {
        let role = self
            .db()
            .call(move |conn| qm_store::agents::role_of(conn, supervisor))
            .await;
        match role {
            Ok(Some((Role::Secondmate, Some(id)))) => id.to_string(),
            _ => "quartermaster".to_owned(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Runners
    // -----------------------------------------------------------------------------------------

    /// Starts the runner of `task` (if none runs) and registers it as its crewmate's wake
    /// consumer.
    pub(crate) fn spawn_runner(&self, task: TaskId, agent: AgentId, start: runner::RunnerStart) {
        if self.is_shutting_down() {
            return;
        }
        let mut runners = self.runners();
        if runners.contains_key(&task) {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = CrewHandle { tx };
        let (activity, _) = broadcast::channel(256);
        let state = Arc::new(RunnerState::new(activity));
        let runner =
            runner::CrewRunner::new(task, agent, self.clone(), rx, Arc::clone(&state), start);
        let join = tokio::spawn(runner.run());
        runners.insert(
            task,
            RunnerEntry {
                agent,
                handle: handle.clone(),
                state,
                join: Some(join),
            },
        );
        drop(runners);
        self.inner
            .deps
            .inbox
            .register(agent, &(Arc::new(handle) as Arc<dyn WakeSink>));
    }

    /// Called by a runner as it ends: forgets it and unregisters its wake consumer.
    pub(crate) fn runner_ended(&self, task: TaskId, agent: AgentId) {
        let removed = self.runners().remove(&task);
        if let Some(entry) = removed {
            debug_assert_eq!(entry.agent, agent, "a runner ended for another crewmate");
            self.inner.deps.inbox.unregister(entry.agent);
        }
        self.changed();
    }

    pub(crate) fn send_to(&self, task: TaskId, input: CrewInput) -> bool {
        let runners = self.runners();
        runners.get(&task).is_some_and(|r| r.handle.send(input))
    }

    /// Asks the runner of `task` to stop its crewmate (close the session, stop its jobs) and
    /// waits for it; `None` when no runner was running.
    pub(crate) async fn stop_runner(&self, task: TaskId) -> Option<StopReport> {
        let (tx, rx) = oneshot::channel();
        let sent = {
            let runners = self.runners();
            match runners.get(&task) {
                Some(r) => {
                    r.state.cancel();
                    r.handle.send(CrewInput::Stop { done: tx })
                }
                None => false,
            }
        };
        if !sent {
            return None;
        }
        rx.await.ok()
    }

    /// The supervisor acted on `task` (for the idle ladder's second wake).
    pub(crate) fn supervisor_acted(&self, task: TaskId) {
        self.send_to(task, CrewInput::SupervisorActed);
    }

    /// Runners with a turn or a job running: what the quit confirmation counts.
    pub fn working_count(&self) -> u32 {
        let n = self
            .runners()
            .values()
            .filter(|r| r.state.is_working())
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Your `Esc` or the task pane's Stop: interrupts the crewmate's running turn, keeping the
    /// session; no idle ladder follows until you speak again.
    pub fn interrupt(&self, task: TaskId) {
        if !self.send_to(task, CrewInput::Interrupt) {
            tracing::debug!(%task, "no crewmate runs for this task");
        }
    }

    /// Your direct message to a crewmate (stored by the API, neutralised): injected into its
    /// running turn, or a turn of its own.
    pub fn deliver_user_message(&self, task: TaskId, message: MessageId) {
        if !self.send_to(task, CrewInput::UserMessage { message }) {
            tracing::warn!(%task, "your message waits: no crewmate runs for this task");
        }
    }

    /// The crewmate's live activity, for the session view.
    pub fn activity(&self, task: TaskId) -> Option<broadcast::Receiver<CrewActivity>> {
        self.runners()
            .get(&task)
            .map(|r| r.state.activity.subscribe())
    }

    /// Quitting: every runner interrupts its turn (its wakes released, so they run again),
    /// closes its session and stops its jobs and setup command; waits up to [`SHUTDOWN_GRACE`]
    /// in all. Idempotent.
    pub async fn shutdown(&self) {
        // `send_replace`: a flag read with `borrow()`, stored even when nobody is subscribed
        // (`send` drops the value then).
        self.inner.shutting_down.send_replace(true);
        let joins: Vec<(TaskId, JoinHandle<()>)> = {
            let mut runners = self.runners();
            runners
                .iter_mut()
                .filter_map(|(task, r)| {
                    r.state.cancel();
                    let _ = r.handle.send(CrewInput::Shutdown);
                    r.join.take().map(|j| (*task, j))
                })
                .collect()
        };
        if joins.is_empty() {
            return;
        }
        let clock = self.clock_arc();
        let deadline = clock.instant() + SHUTDOWN_GRACE;
        for (task, mut join) in joins {
            tokio::select! {
                _ = &mut join => {}
                () = clock.sleep_until(deadline) => {
                    tracing::warn!(%task, "a crewmate didn't stop in time while quitting");
                    join.abort();
                }
            }
        }
    }

    fn spawn_refresh(&self) {
        let weak = Arc::downgrade(&self.inner);
        let clock = self.clock_arc();
        tokio::spawn(async move {
            loop {
                clock.sleep(UI_REFRESH).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let svc = TaskService { inner };
                if svc.is_shutting_down() {
                    return;
                }
                let any = !svc.runners().is_empty();
                if any {
                    svc.changed();
                }
            }
        });
    }

    /// Schedules the re-route tick for queued tasks.
    pub(crate) fn schedule_reroute(&self) {
        let svc = self.clone();
        self.inner.deps.scheduler.after(
            REROUTE_KEY,
            REROUTE_EVERY,
            Box::new(move || Box::pin(async move { start::reroute_tick(&svc).await })),
        );
    }
}

/// The task and crewmate tools, as the core's tool host delegates them.
impl TaskService {
    pub async fn start_crewmate(
        &self,
        caller: &Caller,
        request: StartRequest,
    ) -> Result<TaskStarted, ToolFailure> {
        start::start(self, caller, request).await
    }

    pub async fn message_task(
        &self,
        caller: &Caller,
        task: TaskId,
        text: String,
    ) -> Result<TaskUpdated, ToolFailure> {
        waits::message_task(self, caller, task, text).await
    }

    pub async fn stop_task(
        &self,
        caller: &Caller,
        task: TaskId,
        reason: Option<String>,
    ) -> Result<TaskUpdated, ToolFailure> {
        close::stop_task(self, caller, task, reason).await
    }

    pub async fn set_task_project(
        &self,
        caller: &Caller,
        task: TaskId,
        project: Option<String>,
        folder: Option<String>,
    ) -> Result<TaskUpdated, ToolFailure> {
        relabel::set_task_project(self, caller, task, project, folder).await
    }

    pub async fn close_task(
        &self,
        caller: &Caller,
        task: TaskId,
    ) -> Result<TaskUpdated, ToolFailure> {
        close::close_task(self, caller, task).await
    }

    pub async fn link_tasks(
        &self,
        caller: &Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> Result<Linked, ToolFailure> {
        waits::link_tasks(self, caller, blocker, blocked).await
    }

    pub async fn unlink_tasks(
        &self,
        caller: &Caller,
        blocker: TaskId,
        blocked: TaskId,
    ) -> Result<Unlinked, ToolFailure> {
        waits::unlink_tasks(self, caller, blocker, blocked).await
    }

    pub async fn task_detail(
        &self,
        caller: &Caller,
        task: TaskId,
    ) -> Result<TaskDetail, ToolFailure> {
        lookup::task_detail(self, caller, task).await
    }

    pub async fn find_tasks(
        &self,
        caller: &Caller,
        query: FindTasksArgs,
    ) -> Result<FoundTasks, ToolFailure> {
        lookup::find_tasks(self, caller, query).await
    }

    pub async fn record_report(
        &self,
        caller: &Caller,
        report: ReportArgs,
    ) -> Result<Ack, ToolFailure> {
        report::record(self, caller, report).await
    }

    pub async fn ask_decision(
        &self,
        caller: &Caller,
        question: String,
        options: Vec<String>,
    ) -> Result<Ack, ToolFailure> {
        waits::ask_decision(self, caller, question, options).await
    }

    pub async fn declare_wait(
        &self,
        caller: &Caller,
        wait: DeclaredWait,
    ) -> Result<Ack, ToolFailure> {
        waits::declare_wait(self, caller, wait).await
    }

    pub async fn file_followup(
        &self,
        caller: &Caller,
        followup: FileFollowupArgs,
    ) -> Result<Ack, ToolFailure> {
        waits::file_followup(self, caller, followup).await
    }

    pub async fn delivery_check(&self, caller: &Caller) -> Result<DeliveryCheck, ToolFailure> {
        waits::delivery_check(self, caller).await
    }

    /// Choice 2 on a decision card: a new crewmate retrying `retried` at exactly `tier`, on the
    /// authority of your card choice, with your words in its brief.
    pub async fn start_retry(
        &self,
        retried: TaskId,
        tier: qm_types::domain::Tier,
        words: Option<String>,
    ) -> Result<TaskStarted, ToolFailure> {
        start::start_retry(self, retried, tier, words).await
    }
}

/// The task service's commands and snapshot part: `Esc` on a crewmate, and the quit.
#[derive(Clone, Debug)]
pub struct TasksPart {
    svc: TaskService,
}

impl TasksPart {
    pub fn new(svc: TaskService) -> Self {
        Self { svc }
    }
}

impl ApiPart for TasksPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::Interrupt {
                agent: AgentRef::Crewmate { task },
            } => {
                let task = *task;
                Some(Box::pin(async move {
                    self.svc.interrupt(task);
                    Handled::Unchanged
                }))
            }
            Command::RequestQuit => Some(Box::pin(async move {
                let working = self.svc.working_count();
                if working > 0 {
                    Handled::ConfirmQuit { working }
                } else {
                    self.svc.shutdown().await;
                    Handled::Unchanged
                }
            })),
            Command::Quit => Some(Box::pin(async move {
                self.svc.shutdown().await;
                Handled::Unchanged
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        let _ = snapshot;
        Box::pin(async {})
    }
}

/// A refusal as a tool failure.
pub(crate) fn refuse(reason: impl Into<String>) -> ToolFailure {
    ToolFailure::Refused(Refusal::new(reason))
}

/// A failure reading the task's own commits ([`qm_git::Repo::own_commits`]): a refusal when the
/// branch it targets is missing or shares no history with it, a failure of the app otherwise.
pub(crate) fn own_commits_failure(e: &qm_git::GitError) -> ToolFailure {
    match e {
        qm_git::GitError::NoTargetBranch { .. } | qm_git::GitError::NoMergeBase { .. } => {
            refuse(e.to_string())
        }
        _ => failed(e),
    }
}
