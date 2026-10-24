//! One crewmate's runner.
//!
//! One Tokio task per open task that has a crewmate. It owns the crewmate's `SessionHandle`
//! (opened at phase C, or at the first turn after a restart), is the only reader of its events,
//! and runs one turn at a time. Crewmates have no context block, no action rule and no limits;
//! three kinds of input slip into a running turn: supervisor steering, a `job_exited` wake and
//! your direct message.
//!
//! Idle, the runner waits on its inputs, the wake window (2 s after the last signal, at most 10 s
//! after the first), the watchdog deadline and the engine's events (a job can finish, the session
//! can be suspended or close between turns). Time comes only from the injected clock.
//!
//! A turn: record it and claim its wakes; an app-event turn with nothing to send ends at once as
//! `no_wakes`; assemble the input; read events until `TurnEnded`, dispatching tool calls with the
//! crewmate's `Caller`; commit the turn, its wakes consumed (released on a quit) and its final
//! text kept as a report when it made no clear `report` call.

use super::input::{FirstBlock, assemble, render_wakes};
use super::session::{RoutedSession, crewmate_spec};
use super::start::{PrepareError, gather_for, prepare, routed_session, workplace};
use super::{TaskService, new_wake};
use crate::inbox::Inbox;
use crate::plans;
use crate::skills::Gathered;
use crate::turns::{CLAIM_LIMIT, INTERRUPT_GRACE, WAKE_MAX_HOLD, WAKE_QUIET};
use crate::watchdogs::{SIGNS_RECHECK, TurnEndKind, Watch, perform, signs_of_life};
use jiff::Timestamp;
use qm_engine::supervisor::{ProcessGroup, Signal};
use qm_engine::{
    CloseReason, EngineError, EngineEvent, FailureKind, InputBlock, RateLimitStatus, SessionHandle,
    ToolCallId, ToolResult, TurnInput, TurnOutcome, TurnUsage,
};
use qm_policy::watchdog::SignsOfLife;
use qm_store::agents::{self, CrewUsage, FinishedCrewTurn, REASON_TASK_CLOSED};
use qm_store::inbox as inbox_store;
use qm_store::jobs as job_store;
use qm_store::tasks as task_store;
use qm_store::turns::REASON_APP_QUIT;
use qm_store::wakes::{self, ClaimedWake};
use qm_tools::handler::{Caller, ToolCx, TurnCx};
use qm_types::domain::{
    AgentStatus, EngineKind, PlanMode, Role, StartedBy, TaskStatus, TurnStatus, WakeKind,
};
use qm_types::ids::{AgentId, MessageId, ModelId, PortBlock, SecondmateId, TaskId, TurnId, WakeId};
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// `events.kind` of a tool the crewmate used, as the task pane's activity shows it.
pub const TOOL_EVENT: &str = "task.tool";
/// `events.kind` of the account hitting its limit.
pub const ACCOUNT_LIMIT_EVENT: &str = "task.account_limit";
/// `events.kind` of `EndConversation`.
pub const END_CONVERSATION_EVENT: &str = "task.end_conversation";
/// `events.kind` of a session missing some of the crewmate's tools.
pub const TOOLS_MISSING_EVENT: &str = "task.session_tools_missing";
/// How long signs of life read from the database stay fresh.
const SIGNS_TTL: Duration = Duration::from_secs(5);
/// How long the runner waits before trying again when a turn couldn't be recorded.
const TURN_RETRY: Duration = Duration::from_secs(5);
/// A sign of life is written at most this often.
const TOUCH_EVERY: Duration = Duration::from_mins(1);
/// The Claude engine's prefix for our tools.
const MCP_PREFIX: &str = "mcp__quartermaster__";

/// What the runner is told.
#[derive(Debug)]
pub(crate) enum CrewInput {
    /// A wake for this crewmate was committed.
    Wake,
    /// Your direct message (stored, neutralised).
    UserMessage { message: MessageId },
    /// `Esc` or the task pane's Stop.
    Interrupt,
    /// The supervisor acted on the task (the idle ladder's second wake).
    SupervisorActed,
    /// Close and retire: stop the crewmate, then answer.
    Stop { done: oneshot::Sender<StopReport> },
    /// Quit.
    Shutdown,
}

/// The crewmate was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StopReport {
    /// A turn was running and was interrupted.
    pub interrupted_turn: bool,
}

/// What the crewmate is doing, for the session view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrewActivity {
    TurnStarted,
    /// A line of the crewmate's text.
    Text(String),
    /// A tool it used, as "Bash cargo test auth".
    Tool(String),
    TurnEnded,
}

/// Where a block of a crewmate's input came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// The first turn's brief.
    Brief,
    /// A new session after the old one was lost: the note, then the brief.
    Reopened,
    /// A claimed wake (a `steer` is the supervisor's words).
    Wake,
    /// Your direct message.
    You,
}

/// What happened, as the session recorder hears it.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionRecordKind {
    /// The runner started a turn.
    TurnStarted { started_by: StartedBy },
    /// The runner committed a turn.
    TurnEnded { status: TurnStatus },
    /// A block of input the crewmate was given, at a turn's start or slipped into it. `short` is
    /// a wake's short line (or a reopened session's note); `message` is your message's row.
    Input {
        source: InputSource,
        text: String,
        wake: Option<WakeKind>,
        short: Option<String>,
        message: Option<MessageId>,
    },
    /// Every engine event, as it arrived.
    Engine(EngineEvent),
    /// The result one of our tools gave back.
    ToolResult {
        call: ToolCallId,
        name: String,
        text: String,
        is_error: bool,
    },
}

/// One thing a crewmate's runner saw, for its session view.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecord {
    pub task: TaskId,
    pub agent: AgentId,
    pub turn: Option<TurnId>,
    pub at: Timestamp,
    pub kind: SessionRecordKind,
}

/// Hears everything a crewmate's runner sees. `record` is called inline by the runner, so it must
/// not block, and must keep everything (an unbounded channel).
pub trait SessionSink: Send + Sync + fmt::Debug {
    fn record(&self, record: SessionRecord);
}

impl TaskService {
    /// The sink every runner hands its session records to (the recorder). Set once; a second
    /// call is ignored.
    pub fn set_session_sink(&self, sink: Arc<dyn SessionSink>) {
        if self.inner.sink.set(sink).is_err() {
            tracing::warn!("the task service's session sink was set twice; the first one stays");
        }
    }

    fn session_sink(&self) -> Option<&Arc<dyn SessionSink>> {
        self.inner.sink.get()
    }
}

/// What the service can see of a runner.
#[derive(Debug)]
pub(crate) struct RunnerState {
    turn_running: AtomicBool,
    jobs_running: AtomicU32,
    pub(crate) activity: broadcast::Sender<CrewActivity>,
    cancel: watch::Sender<bool>,
}

impl RunnerState {
    pub(crate) fn new(activity: broadcast::Sender<CrewActivity>) -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            turn_running: AtomicBool::new(false),
            jobs_running: AtomicU32::new(0),
            activity,
            cancel,
        }
    }

    /// A turn or a job is running.
    pub(crate) fn is_working(&self) -> bool {
        self.turn_running.load(Ordering::Relaxed) || self.jobs_running.load(Ordering::Relaxed) > 0
    }

    /// Stops a long step (the setup command) at once: the task is being stopped or the app quits.
    pub(crate) fn cancel(&self) {
        // `send_replace`: stored even before a setup command subscribes (`send` drops the
        // value when nobody is), so `next_turn` and a later `prepare` both see it.
        self.cancel.send_replace(true);
    }

    fn cancelled(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }
}

/// How a runner begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunnerStart {
    /// Phase C, then the first turn with the brief.
    Prepare,
    /// After a restart: the `restarted` wake is posted; the next turn resumes the session.
    Resume,
    /// Idle; with the idle ladder back for the spell that began with this turn.
    Idle { idle_spell: Option<(TurnId, bool)> },
}

/// What ends the runner.
enum Ending {
    Stop(oneshot::Sender<StopReport>),
    Shutdown,
    /// The task failed; the runner hands retirement to its own task once it has stopped.
    Failed(String),
}

/// What a finished turn came to.
struct TurnResult {
    status: TurnStatus,
    reason: Option<String>,
    usage: TurnUsage,
    final_text: String,
    clear_report: bool,
    end_kind: TurnEndKind,
    /// Claims whose live injection failed; release only these before consuming the turn's wakes.
    undelivered_wakes: Vec<WakeId>,
}

/// A wake window: ready 2 s after the last signal, at most 10 s after the first.
#[derive(Clone, Copy, Debug, Default)]
struct WakeWindow {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl WakeWindow {
    fn note(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn ready_at(&self) -> Option<Instant> {
        match (self.first, self.last) {
            (Some(first), Some(last)) => Some((last + WAKE_QUIET).min(first + WAKE_MAX_HOLD)),
            _ => None,
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// One crewmate's runner.
pub(crate) struct CrewRunner {
    task: TaskId,
    agent: AgentId,
    svc: TaskService,
    rx: mpsc::UnboundedReceiver<CrewInput>,
    state: Arc<RunnerState>,
    start: Option<RunnerStart>,
    session: Option<SessionHandle>,
    routed: Option<(RoutedSession, PortBlock)>,
    gathered: Option<Gathered>,
    /// What the next turn starts with.
    first: FirstBlock,
    /// The brief, for a reopened session.
    brief: Option<String>,
    /// The next session is a new one, with this note.
    reopen: Option<String>,
    watch: Watch,
    cut_off_until: Option<Instant>,
    wakes: WakeWindow,
    messages: VecDeque<MessageId>,
    supervisor_label: String,
    secondmate: Option<SecondmateId>,
    model: Option<ModelId>,
    last_touch: Option<Instant>,
    ending: Option<Ending>,
    /// A stop arrived while a turn ran, and interrupted it.
    stopped_mid_turn: bool,
    /// The signs of life last read, and when.
    signs: Option<(Instant, SignsOfLife)>,
    /// A plan task's: the plan mode the open session was started in.
    plan: Option<PlanMode>,
}

impl CrewRunner {
    pub(crate) fn new(
        task: TaskId,
        agent: AgentId,
        svc: TaskService,
        rx: mpsc::UnboundedReceiver<CrewInput>,
        state: Arc<RunnerState>,
        start: RunnerStart,
    ) -> Self {
        Self {
            task,
            agent,
            svc,
            rx,
            state,
            start: Some(start),
            session: None,
            routed: None,
            gathered: None,
            first: FirstBlock::None,
            brief: None,
            reopen: None,
            watch: Watch::default(),
            cut_off_until: None,
            wakes: WakeWindow::default(),
            messages: VecDeque::new(),
            supervisor_label: "quartermaster".to_owned(),
            secondmate: None,
            model: None,
            last_touch: None,
            ending: None,
            stopped_mid_turn: false,
            signs: None,
            plan: None,
        }
    }

    fn clock(&self) -> Arc<dyn qm_types::clock::Clock> {
        self.svc.clock_arc()
    }

    /// Hands a session record to the sink, if one is set; never blocks.
    fn record(&self, turn: Option<TurnId>, kind: impl FnOnce() -> SessionRecordKind) {
        if let Some(sink) = self.svc.session_sink() {
            sink.record(SessionRecord {
                task: self.task,
                agent: self.agent,
                turn,
                at: self.svc.clock().now(),
                kind: kind(),
            });
        }
    }

    /// An engine event, for the session view.
    fn record_engine(&self, turn: Option<TurnId>, ev: &EngineEvent) {
        self.record(turn, || SessionRecordKind::Engine(ev.clone()));
    }

    /// Claimed wakes, as the crewmate is given them.
    fn record_wakes(&self, turn: TurnId, claimed: &[ClaimedWake]) {
        for wake in claimed {
            self.record(Some(turn), || {
                let text = |key: &str| {
                    wake.payload
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                SessionRecordKind::Input {
                    source: InputSource::Wake,
                    text: text("text").unwrap_or_else(|| wake.payload.to_string()),
                    wake: Some(wake.kind),
                    short: text("short"),
                    message: None,
                }
            });
        }
    }

    /// A turn's start and everything it begins with, in the order the crewmate reads it.
    fn record_turn_start(
        &self,
        turn: TurnId,
        started_by: StartedBy,
        first: &FirstBlock,
        claimed: &[ClaimedWake],
        message_text: Option<&str>,
    ) {
        self.record(Some(turn), || SessionRecordKind::TurnStarted { started_by });
        let opening = match first {
            FirstBlock::Brief(brief) => Some((brief, InputSource::Brief, None)),
            FirstBlock::Reopened { why, brief } => Some((brief, InputSource::Reopened, Some(why))),
            FirstBlock::None => None,
        };
        if let Some((text, source, short)) = opening {
            self.record(Some(turn), || SessionRecordKind::Input {
                source,
                text: text.clone(),
                wake: None,
                short: short.cloned(),
                message: None,
            });
        }
        self.record_wakes(turn, claimed);
        if let (StartedBy::User { message }, Some(text)) = (started_by, message_text) {
            self.record_message(Some(turn), message, text);
        }
    }

    /// Your message, as the crewmate is given it.
    fn record_message(&self, turn: Option<TurnId>, message: MessageId, text: &str) {
        self.record(turn, || SessionRecordKind::Input {
            source: InputSource::You,
            text: text.to_owned(),
            wake: None,
            short: None,
            message: Some(message),
        });
    }

    /// Runs until the task closes or the app quits.
    pub(crate) async fn run(mut self) {
        self.begin().await;
        while self.ending.is_none() {
            if let Some(next) = self.next_turn().await {
                self.turn(next).await;
                self.move_workplace().await;
                continue;
            }
            self.idle().await;
        }
        self.finish().await;
    }

    // -----------------------------------------------------------------------------------------
    // Beginning and ending
    // -----------------------------------------------------------------------------------------

    async fn begin(&mut self) {
        let task = self.task;
        let who = self
            .svc
            .db()
            .call(move |conn| {
                let row = task_store::load(conn, task)?;
                let role = match &row {
                    Some(r) => agents::role_of(conn, r.supervisor)?,
                    None => None,
                };
                Ok((row, role))
            })
            .await;
        if let Ok((Some(row), role)) = who {
            self.supervisor_label = self.svc.supervisor_label(row.supervisor).await;
            self.secondmate = match role {
                Some((Role::Secondmate, sm)) => sm,
                _ => None,
            };
        }
        match self.start.take() {
            Some(RunnerStart::Prepare) => {
                match prepare(&self.svc, self.task, self.state.cancelled()).await {
                    Ok(prepared) => {
                        self.routed = Some((prepared.routed, prepared.ports));
                        self.gathered = Some(prepared.gathered);
                        self.brief = Some(prepared.brief.clone());
                        self.first = FirstBlock::Brief(prepared.brief);
                        self.set_agent(AgentStatus::Starting).await;
                    }
                    Err(PrepareError::Cancelled) => {}
                    Err(PrepareError::Failed(reason)) => {
                        self.ending = Some(Ending::Failed(reason));
                    }
                }
            }
            Some(RunnerStart::Idle {
                idle_spell: Some((turn, failed_at_provider)),
            }) => {
                let clock = self.clock();
                self.watch.resume_idle(&*clock, turn, failed_at_provider);
            }
            Some(RunnerStart::Resume | RunnerStart::Idle { idle_spell: None }) | None => {}
        }
        // Catch up with wakes committed while nobody listened (the inbox also signals once).
        self.check_pending().await;
    }

    /// The signs of life, read from the database at most every [`SIGNS_TTL`] (a busy turn sends
    /// many events).
    async fn signs(&mut self) -> SignsOfLife {
        let clock = self.clock();
        let now = clock.instant();
        if let Some((at, signs)) = self.signs
            && now.saturating_duration_since(at) < SIGNS_TTL
        {
            return signs;
        }
        let signs = signs_of_life(
            self.svc.db(),
            &*clock,
            self.task,
            self.agent,
            self.cut_off_until,
        )
        .await;
        self.signs = Some((now, signs));
        signs
    }

    /// Forgets the signs of life read, after something that changes them.
    fn stale_signs(&mut self) {
        self.signs = None;
    }

    async fn check_pending(&mut self) {
        let agent = self.agent;
        if let Ok(n) = self
            .svc
            .db()
            .call(move |conn| inbox_store::pending_count(conn, agent))
            .await
            && n > 0
        {
            let now = self.clock().instant();
            self.wakes.note(now);
        }
    }

    /// Stops the crewmate and answers whoever asked.
    async fn finish(mut self) {
        let interrupted = self.stopped_mid_turn;
        // A quit closes the session and keeps its transcript, so the crewmate resumes its
        // conversation at the next start. Every other exit is terminal, including failure: fail()
        // retires the task after this runner unregisters, when no live handle remains to forget
        // its transcript.
        if matches!(self.ending, Some(Ending::Shutdown) | None) {
            self.end_session_with(SessionHandle::close).await;
        } else {
            self.end_session().await;
        }
        let task = self.task;
        let agent = self.agent;
        self.svc.runner_ended(task, agent);
        match self.ending.take() {
            Some(Ending::Stop(done)) => {
                let _ = done.send(StopReport {
                    interrupted_turn: interrupted,
                });
            }
            Some(Ending::Failed(reason)) => {
                let svc = self.svc.clone();
                tokio::spawn(async move {
                    svc.fail(task, reason).await;
                });
            }
            Some(Ending::Shutdown) | None => {}
        }
    }

    /// Forgets the session (waiting for it), stops every job or
    /// setup group still open for the task, and marks the crewmate stopped.
    async fn end_session(&mut self) {
        self.end_session_with(SessionHandle::forget).await;
    }

    /// As [`Self::end_session`], ending the session `how`: `forget` drops its transcript, `close`
    /// keeps it to resume.
    async fn end_session_with(&mut self, how: fn(&SessionHandle) -> Result<(), EngineError>) {
        let clock = self.clock();
        if let Some(mut handle) = self.session.take() {
            let _ = how(&handle);
            let deadline = clock.instant() + INTERRUPT_GRACE;
            loop {
                tokio::select! {
                    ev = handle.next_event() => {
                        if let Some(ev) = &ev {
                            self.record_engine(None, ev);
                        }
                        match ev {
                            None | Some(EngineEvent::SessionClosed { .. }) => break,
                            Some(EngineEvent::JobFinished { job, exit_code, tail, agent_told }) => {
                                self.on_job_finished(job.to_string(), exit_code, tail, agent_told).await;
                            }
                            Some(EngineEvent::SessionReady { session, .. }) => {
                                self.store_session(Some(&session)).await;
                            }
                            Some(_) => {}
                        }
                    }
                    () = clock.sleep_until(deadline) => {
                        tracing::warn!(task = %self.task, "the crewmate's session didn't close in time");
                        break;
                    }
                }
            }
        }
        stop_open_groups(&self.svc, self.task, Some(self.agent)).await;
        let agent = self.agent;
        let at = clock.now();
        let _ = self
            .svc
            .db()
            .call(move |conn| job_store::close_running(conn, agent, at).map(drop))
            .await;
        self.state.jobs_running.store(0, Ordering::Relaxed);
        self.set_agent(AgentStatus::Stopped).await;
    }

    // -----------------------------------------------------------------------------------------
    // Idle
    // -----------------------------------------------------------------------------------------

    /// What starts the next turn, if one is ready now.
    async fn next_turn(&mut self) -> Option<NextTurn> {
        // A stop or the quit is on its way: start nothing new.
        if self.ending.is_some() || *self.state.cancel.borrow() {
            return None;
        }
        if let Some(message) = self.messages.pop_front() {
            return Some(NextTurn::User(message));
        }
        if !matches!(self.first, FirstBlock::None) {
            return Some(NextTurn::App);
        }
        let now = self.clock().instant();
        if self.wakes.ready_at().is_some_and(|at| now >= at) {
            self.wakes.clear();
            let agent = self.agent;
            let pending = self
                .svc
                .db()
                .call(move |conn| inbox_store::pending_count(conn, agent))
                .await
                .unwrap_or(0);
            if pending > 0 {
                return Some(NextTurn::App);
            }
        }
        None
    }

    /// Waits between turns for an input, an event, the wake window or a watchdog deadline.
    async fn idle(&mut self) {
        let clock = self.clock();
        let signs = self.signs().await;
        for step in self.watch.poll(&*clock, &signs) {
            perform(&self.svc, self.task, self.agent, step).await;
        }
        let mut deadline = self.watch.next_deadline(&*clock, &signs);
        if self.watch.active() {
            let recheck = clock.instant() + SIGNS_RECHECK;
            deadline = Some(deadline.map_or(recheck, |d| d.min(recheck)));
        }
        if let Some(ready) = self.wakes.ready_at() {
            deadline = Some(deadline.map_or(ready, |d| d.min(ready)));
        }
        let sleep = async {
            match deadline {
                Some(d) => clock.sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        let has_session = self.session.is_some();
        tokio::select! {
            input = self.rx.recv() => self.on_idle_input(input),
            ev = next_event(self.session.as_mut()), if has_session => self.on_idle_event(ev).await,
            () = sleep => {}
        }
    }

    fn on_idle_input(&mut self, input: Option<CrewInput>) {
        self.stale_signs();
        let now = self.clock().instant();
        match input {
            None | Some(CrewInput::Shutdown) => self.ending = Some(Ending::Shutdown),
            Some(CrewInput::Stop { done }) => self.ending = Some(Ending::Stop(done)),
            Some(CrewInput::Wake) => self.wakes.note(now),
            Some(CrewInput::UserMessage { message }) => self.messages.push_back(message),
            Some(CrewInput::Interrupt) => {}
            Some(CrewInput::SupervisorActed) => self.watch.supervisor_acted(),
        }
    }

    async fn on_idle_event(&mut self, ev: Option<EngineEvent>) {
        if let Some(ev) = &ev {
            self.record_engine(None, ev);
        }
        match ev {
            None => self.session = None,
            Some(EngineEvent::SessionClosed { reason }) => {
                self.session = None;
                match reason {
                    CloseReason::Crashed { .. } => self.set_agent(AgentStatus::Crashed).await,
                    CloseReason::EndConversation => {
                        self.reopen = Some("Your previous session ended.".to_owned());
                    }
                    CloseReason::ResumeFailed { message } => {
                        self.reopen = Some(format!(
                            "Your previous session couldn't be resumed ({message})."
                        ));
                    }
                    CloseReason::Requested => {}
                }
            }
            Some(EngineEvent::SessionReady { session, .. }) => {
                self.store_session(Some(&session)).await;
            }
            Some(EngineEvent::JobStarted {
                job,
                command,
                log_path,
                ran_for_ms,
            }) => {
                self.on_job_started(job.to_string(), command, log_path, ran_for_ms)
                    .await;
            }
            Some(EngineEvent::JobFinished {
                job,
                exit_code,
                tail,
                agent_told,
            }) => {
                // The wake signals this runner through the inbox; it starts a turn after the
                // window (the session resumes if it was suspended). A job the agent was told about
                // posts none: its harness starts that turn itself (`TurnStarted` below).
                self.on_job_finished(job.to_string(), exit_code, tail, agent_told)
                    .await;
            }
            Some(EngineEvent::RateLimit { info }) => {
                let _ = self.on_rate_limit(&info, None).await;
            }
            Some(EngineEvent::TurnStarted) => {
                // Claude started a turn by itself after a job notification: record it as an
                // app-event turn and take it like any other.
                self.turn_in_progress().await;
            }
            Some(_) => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Turns
    // -----------------------------------------------------------------------------------------

    /// A plan task's plan mode from its project's autonomy now; `None` for any other task.
    async fn live_plan_mode(&self) -> Result<Option<PlanMode>, String> {
        let task = self.task;
        let row = self
            .svc
            .db()
            .call(move |conn| task_store::load(conn, task))
            .await
            .map_err(|e| e.to_string())?;
        match row {
            Some(row) => self.svc.live_plan_mode(&row).await.map_err(|e| e.render()),
            None => Ok(None),
        }
    }

    /// Before a turn: a plan task's session started in another mode than the live one ends, and
    /// the turn opens a fresh one in the new mode, with its tools, prompt and skill.
    async fn check_plan_mode(&mut self) {
        let Some(running) = self.plan else {
            return;
        };
        if self.session.is_none() {
            return;
        }
        let Ok(Some(live)) = self.live_plan_mode().await else {
            return;
        };
        if live == running {
            return;
        }
        tracing::info!(task = %self.task, from = %running, to = %live, "restarting a planner in its new plan mode");
        self.end_session().await;
        self.reopen = Some(mode_changed_note(live));
    }

    /// Before a turn: when the crewmate's supervisor moved it to another task, its folders follow
    /// now (`relabel::follow_task`), unless a background job is running, which may be writing to
    /// them. The session closes first and reopens on the new folders, resuming the conversation.
    async fn follow_task(&mut self) {
        if self.state.jobs_running.load(Ordering::Relaxed) > 0 {
            return;
        }
        let task = self.task;
        let moving = self
            .svc
            .db()
            .call(move |conn| {
                Ok(task_store::load(conn, task)?.is_some_and(|r| r.label != r.folder_label))
            })
            .await
            .unwrap_or(false);
        if !moving {
            return;
        }
        self.end_session_with(SessionHandle::close).await;
        if let Err(e) = super::relabel::follow_task(&self.svc, task).await {
            tracing::warn!(%task, error = %e, "a crewmate's folders couldn't follow it to its new task yet");
        }
    }

    /// Between turns: a move of the crewmate's workplace its supervisor asked for
    /// (`moving::set_task_project`) is made now, unless a background job is running in the old
    /// folder. The session closes first and reopens in the new folder, resuming the conversation;
    /// its skills and instructions are gathered again for the new project.
    async fn move_workplace(&mut self) {
        if self.ending.is_some() || self.state.jobs_running.load(Ordering::Relaxed) > 0 {
            return;
        }
        if !super::moving::is_waiting(&self.svc, self.task).await {
            return;
        }
        self.end_session_with(SessionHandle::close).await;
        let applied =
            super::moving::apply(&self.svc, self.task, Some(self.state.cancelled())).await;
        if applied == super::moving::Applied::Moved {
            self.gathered = None;
            self.brief = None;
        }
    }

    /// Opens the session if none is open: resuming the stored one unless a new one is needed.
    async fn ensure_session(&mut self) -> Result<(), String> {
        if self.session.is_some() {
            return Ok(());
        }
        if self.routed.is_none() {
            let (routed, ports, _) = routed_session(&self.svc, self.task)
                .await
                .map_err(|e| e.to_string())?;
            self.routed = Some((routed, ports));
        }
        if self.gathered.is_none() {
            self.gathered = Some(
                gather_for(&self.svc, self.task)
                    .await
                    .map_err(|e| e.to_string())?,
            );
        }
        let agent = self.agent;
        let stored = self
            .svc
            .db()
            .call(move |conn| agents::load(conn, agent))
            .await
            .map_err(|e| e.to_string())?
            .and_then(|a| a.session_ref);
        // A plan task's session runs in the mode the project's autonomy gives now; a session
        // started in another mode (the app was down when it changed) is not resumed.
        let plan = self.live_plan_mode().await?;
        let paths = self.svc.paths(self.task).await.map_err(|e| e.to_string())?;
        if let Some(mode) = plan {
            let dir = paths.plan.clone();
            let before = tokio::task::spawn_blocking(move || {
                plans::update(&dir, |s| s.session_mode.replace(mode))
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
            .and_then(|(_, before)| before);
            if before.is_some_and(|b| b != mode) && self.reopen.is_none() {
                self.reopen = Some(mode_changed_note(mode));
            }
        }
        self.plan = plan;
        let fresh = self.reopen.is_some();
        let resume = if fresh { None } else { stored };
        if fresh {
            self.store_session(None).await;
            let why = self.reopen.take().unwrap_or_default();
            let brief = match &self.brief {
                Some(b) => b.clone(),
                None => self.saved_brief().await,
            };
            self.first = FirstBlock::Reopened { why, brief };
        }
        let (Some((routed, ports)), Some(gathered)) = (&self.routed, &self.gathered) else {
            return Err("the crewmate's session can't be described".to_owned());
        };
        let place = workplace(&self.svc, self.task)
            .await
            .map_err(|e| e.to_string())?;
        let with_skill;
        let gathered = match plan {
            Some(mode) => {
                with_skill = crate::skills::with_plan_skill(gathered, &paths.skills, mode)
                    .map_err(|e| e.to_string())?;
                &with_skill
            }
            None => gathered,
        };
        let told = super::session::Told { gathered, plan };
        let mut spec = crewmate_spec(agent, routed, &paths, &place, *ports, told, resume);
        spec.protected_roots = self.svc.deps().data_dir.protected_roots();
        let engine =
            self.svc.deps().engines.get(&routed.engine).ok_or_else(|| {
                format!("the {} engine isn't available on this Mac", routed.engine)
            })?;
        let tap = self
            .svc
            .deps()
            .monitor
            .tap(qm_types::api::AgentRef::Crewmate { task: self.task });
        let handle = engine.open(spec).map_err(|e| e.to_string())?.observed(tap);
        self.model = Some(routed.model.clone());
        self.session = Some(handle);
        Ok(())
    }

    /// The brief as saved, for a reopened session after a restart.
    async fn saved_brief(&self) -> String {
        let path = self
            .svc
            .deps()
            .data_dir
            .crewmate_root(self.task)
            .join(qm_types::data_dir::BRIEF);
        tokio::fs::read_to_string(&path).await.unwrap_or_default()
    }

    async fn store_session(&self, session: Option<&qm_types::ids::SessionRef>) {
        let agent = self.agent;
        let session = session.cloned();
        let at = self.clock().now();
        let _ = self
            .svc
            .db()
            .call(move |conn| agents::set_session(conn, agent, session.as_ref(), at))
            .await;
    }

    async fn set_agent(&self, status: AgentStatus) {
        let agent = self.agent;
        let at = self.clock().now();
        if let Err(e) = self
            .svc
            .write(move |tx, _| agents::set_status(tx, agent, status, at))
            .await
        {
            tracing::warn!(task = %self.task, error = %e, "couldn't record the crewmate's status");
        }
    }

    async fn touch(&mut self) {
        let now = self.clock().instant();
        if self
            .last_touch
            .is_some_and(|t| now.saturating_duration_since(t) < TOUCH_EVERY)
        {
            return;
        }
        self.last_touch = Some(now);
        let agent = self.agent;
        let at = self.clock().now();
        let _ = self
            .svc
            .db()
            .call(move |conn| agents::touch(conn, agent, at))
            .await;
    }

    /// Runs one turn.
    async fn turn(&mut self, next: NextTurn) {
        self.check_plan_mode().await;
        self.follow_task().await;
        self.move_workplace().await;
        if let Err(reason) = self.ensure_session().await {
            self.ending = Some(Ending::Failed(format!(
                "the crewmate's session couldn't be opened: {reason}"
            )));
            return;
        }
        let (started_by, message_text) = match next {
            NextTurn::User(message) => {
                let text = self
                    .svc
                    .db()
                    .call(move |conn| qm_store::messages::text(conn, message))
                    .await
                    .ok()
                    .flatten();
                (StartedBy::User { message }, text)
            }
            NextTurn::App => (StartedBy::AppEvent, None),
        };
        let agent = self.agent;
        let task = self.task;
        let at = self.clock().now();
        let model = self.model.clone();
        let started = self
            .svc
            .db()
            .call(move |conn| {
                let turn = agents::start_crew_turn(conn, agent, started_by, model.as_ref(), at)?;
                let claimed = wakes::claim_wakes(conn, agent, turn, at, CLAIM_LIMIT)?;
                Ok((turn, claimed))
            })
            .await;
        let (turn, claimed) = match started {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(%task, error = %e, "couldn't record the crewmate's turn");
                // Try again after a pause rather than spinning; inputs wait in the channel.
                if let StartedBy::User { message } = started_by {
                    self.messages.push_front(message);
                }
                self.clock().sleep(TURN_RETRY).await;
                return;
            }
        };
        if started_by == StartedBy::AppEvent
            && matches!(self.first, FirstBlock::None)
            && claimed.is_empty()
        {
            self.commit(
                turn,
                &TurnResult {
                    status: TurnStatus::Completed,
                    reason: Some(qm_store::turns::NO_WAKES.to_owned()),
                    usage: TurnUsage::default(),
                    final_text: String::new(),
                    clear_report: false,
                    end_kind: TurnEndKind::Other,
                    undelivered_wakes: Vec::new(),
                },
                false,
            )
            .await;
            return;
        }
        self.end_outside_wait().await;
        let first = std::mem::replace(&mut self.first, FirstBlock::None);
        self.record_turn_start(turn, started_by, &first, &claimed, message_text.as_deref());
        let input = assemble(
            &first,
            &claimed,
            message_text.as_deref(),
            &self.supervisor_label,
        );
        let result = self
            .drive(
                turn,
                started_by,
                input,
                &first,
                &claimed,
                message_text.as_deref(),
            )
            .await;
        let release = matches!(self.ending, Some(Ending::Shutdown));
        self.commit(turn, &result, release).await;
        self.after_turn(turn, &result).await;
    }

    /// A turn the engine started by itself: recorded as an app-event turn, any pending
    /// live wakes claimed into it.
    async fn turn_in_progress(&mut self) {
        let agent = self.agent;
        let at = self.clock().now();
        let model = self.model.clone();
        let Ok(turn) = self
            .svc
            .db()
            .call(move |conn| {
                agents::start_crew_turn(conn, agent, StartedBy::AppEvent, model.as_ref(), at)
            })
            .await
        else {
            return;
        };
        self.record(Some(turn), || SessionRecordKind::TurnStarted {
            started_by: StartedBy::AppEvent,
        });
        let result = self.read_turn(turn, StartedBy::AppEvent, true).await;
        let release = matches!(self.ending, Some(Ending::Shutdown));
        self.commit(turn, &result, release).await;
        self.after_turn(turn, &result).await;
    }

    /// An outside wait lasts until the crewmate's next turn starts, whatever starts it.
    async fn end_outside_wait(&self) {
        let task = self.task;
        let at = self.clock().now();
        let _ = self
            .svc
            .write(move |tx, _| {
                if let Some(row) = task_store::load(tx, task)?
                    && row.status == TaskStatus::Waiting
                    && row
                        .waiting_on
                        .as_ref()
                        .is_some_and(|w| w.get("external").is_some())
                {
                    task_store::set_status(tx, task, TaskStatus::Waiting, TaskStatus::Running, at)?;
                }
                Ok(())
            })
            .await;
    }

    /// Sends the input and reads the turn; a session that can't be resumed is reopened once with
    /// the same input behind a `Reopened` note.
    async fn drive(
        &mut self,
        turn: TurnId,
        started_by: StartedBy,
        input: TurnInput,
        first: &FirstBlock,
        claimed: &[ClaimedWake],
        message: Option<&str>,
    ) -> TurnResult {
        let mut input = input;
        let mut reopened = false;
        loop {
            let sent = self
                .session
                .as_ref()
                .is_some_and(|h| h.start_turn(input.clone()).is_ok());
            if !sent {
                return failed_turn("the crewmate's session is gone");
            }
            let result = self.read_turn(turn, started_by, false).await;
            if result.reason.as_deref() != Some(RESUME_FAILED) {
                return result;
            }
            // The input never arrived.
            if reopened {
                self.ending = Some(Ending::Failed(
                    "the crewmate's session couldn't be opened twice".to_owned(),
                ));
                return failed_turn("the crewmate's session couldn't be resumed");
            }
            reopened = true;
            self.reopen = Some("Your previous session couldn't be resumed.".to_owned());
            if let Err(reason) = self.ensure_session().await {
                self.ending = Some(Ending::Failed(format!(
                    "the crewmate's session couldn't be opened: {reason}"
                )));
                return failed_turn("the crewmate's session couldn't be opened");
            }
            let brief = match first {
                FirstBlock::Brief(b) | FirstBlock::Reopened { brief: b, .. } => b.clone(),
                FirstBlock::None => match &self.brief {
                    Some(b) => b.clone(),
                    None => self.saved_brief().await,
                },
            };
            let reopen_first = match std::mem::replace(&mut self.first, FirstBlock::None) {
                FirstBlock::Reopened { why, .. } => FirstBlock::Reopened { why, brief },
                _ => FirstBlock::Reopened {
                    why: "Your previous session couldn't be resumed.".to_owned(),
                    brief,
                },
            };
            if let FirstBlock::Reopened { why, brief } = &reopen_first {
                self.record(Some(turn), || SessionRecordKind::Input {
                    source: InputSource::Reopened,
                    text: brief.clone(),
                    wake: None,
                    short: Some(why.clone()),
                    message: None,
                });
            }
            input = assemble(&reopen_first, claimed, message, &self.supervisor_label);
        }
    }

    /// Reads one turn's events until it ends. `already_started` is a turn the engine began by
    /// itself.
    async fn read_turn(
        &mut self,
        turn: TurnId,
        started_by: StartedBy,
        already_started: bool,
    ) -> TurnResult {
        let mut undelivered = Vec::new();
        let mut result = self
            .read_turn_events(turn, started_by, already_started, &mut undelivered)
            .await;
        result.undelivered_wakes = undelivered;
        result
    }

    #[allow(clippy::too_many_lines)]
    async fn read_turn_events(
        &mut self,
        turn: TurnId,
        started_by: StartedBy,
        already_started: bool,
        undelivered: &mut Vec<WakeId>,
    ) -> TurnResult {
        let clock = self.clock();
        let mut turn_started = false;
        // A resume can fail before TurnStarted: its replacement resends only the original input.
        // After an interrupt or EndConversation, leave new input for the next turn instead.
        let mut accepting_input = already_started;
        let mut clear_report = false;
        let mut end_conversation = false;
        let mut interrupted_by_you = false;
        let mut interrupt_deadline: Option<Instant> = None;
        let mut account_reset: Option<Timestamp> = None;
        let mut inputs_open = true;
        if already_started {
            self.on_turn_started(turn).await;
            turn_started = true;
            self.claim_live_wakes(turn, accepting_input, undelivered)
                .await;
        }
        loop {
            let signs = self.signs().await;
            for step in self.watch.poll(&*clock, &signs) {
                perform(&self.svc, self.task, self.agent, step).await;
            }
            let mut deadline = self.watch.next_deadline(&*clock, &signs);
            if self.watch.active() {
                let recheck = clock.instant() + SIGNS_RECHECK;
                deadline = Some(deadline.map_or(recheck, |d| d.min(recheck)));
            }
            if let Some(i) = interrupt_deadline {
                deadline = Some(deadline.map_or(i, |d| d.min(i)));
            }
            let sleep = async {
                match deadline {
                    Some(d) => clock.sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let Some(handle) = self.session.as_mut() else {
                return failed_turn("the crewmate's session is gone");
            };
            tokio::select! {
                biased;
                // Buffered builtin activity must not starve steering or an explicit interrupt.
                input = self.rx.recv(), if inputs_open => {
                    // A disconnected receiver stays ready forever; still read the engine's end.
                    inputs_open = input.is_some();
                    match input {
                        None | Some(CrewInput::Shutdown) => {
                            accepting_input = false;
                            if self.ending.is_none() {
                                self.ending = Some(Ending::Shutdown);
                            }
                            interrupt_deadline.get_or_insert_with(|| {
                                if let Some(h) = self.session.as_ref() { let _ = h.interrupt(); }
                                clock.instant() + INTERRUPT_GRACE
                            });
                        }
                        Some(CrewInput::Stop { done }) => {
                            accepting_input = false;
                            self.ending = Some(Ending::Stop(done));
                            self.stopped_mid_turn = true;
                            interrupt_deadline.get_or_insert_with(|| {
                                if let Some(h) = self.session.as_ref() { let _ = h.interrupt(); }
                                clock.instant() + INTERRUPT_GRACE
                            });
                        }
                        Some(CrewInput::Interrupt) => {
                            accepting_input = false;
                            interrupted_by_you = true;
                            interrupt_deadline.get_or_insert_with(|| {
                                if let Some(h) = self.session.as_ref() { let _ = h.interrupt(); }
                                clock.instant() + INTERRUPT_GRACE
                            });
                        }
                        Some(CrewInput::Wake) => {
                            self.wakes.note(clock.instant());
                            self.claim_live_wakes(turn, accepting_input, undelivered).await;
                        }
                        Some(CrewInput::UserMessage { message }) => {
                            if accepting_input && self.ending.is_none() && !*self.state.cancel.borrow() {
                                self.inject_message(message).await;
                            } else {
                                self.messages.push_back(message);
                            }
                        }
                        Some(CrewInput::SupervisorActed) => self.watch.supervisor_acted(),
                    }
                }
                ev = handle.next_event() => {
                    let Some(ev) = ev else {
                        self.session = None;
                        self.set_agent(AgentStatus::Crashed).await;
                        return failed_turn("the crewmate's session closed");
                    };
                    self.record_engine(Some(turn), &ev);
                    match ev {
                        EngineEvent::SessionReady { session, init, .. } => {
                            self.store_session(Some(&session)).await;
                            self.check_tools(&init.tools).await;
                        }
                        EngineEvent::TurnStarted => {
                            turn_started = true;
                            accepting_input = interrupt_deadline.is_none() && !end_conversation;
                            self.on_turn_started(turn).await;
                            self.claim_live_wakes(turn, accepting_input, undelivered).await;
                        }
                        EngineEvent::TextLine { text } => {
                            self.watch.engine_event(&*clock);
                            let _ = self.state.activity.send(CrewActivity::Text(text));
                            self.touch().await;
                        }
                        EngineEvent::BuiltinToolUse { name, summary } => {
                            self.watch.engine_event(&*clock);
                            let line = format!("{name} {summary}").trim().to_owned();
                            self.tool_event(turn, line).await;
                            self.touch().await;
                        }
                        EngineEvent::ModelCallFinished { usage, .. } => {
                            self.watch.engine_event(&*clock);
                            self.call_tokens(turn, &usage).await;
                            self.touch().await;
                        }
                        EngineEvent::Activity { .. }
                        | EngineEvent::ModelCallStarted { .. }
                        | EngineEvent::Compacted { .. }
                        | EngineEvent::Retrying { .. } => {
                            self.watch.engine_event(&*clock);
                            self.touch().await;
                        }
                        EngineEvent::ToolCall { call, name, args } => {
                            self.watch.engine_event(&*clock);
                            let bare = name.strip_prefix(MCP_PREFIX).unwrap_or(&name).to_owned();
                            let result = self.dispatch(turn, started_by, &bare, args).await;
                            // Dispatch is inline: a signal may still be queued, or arrive while
                            // the tool awaits. Read durable steering before unblocking the engine.
                            self.claim_live_wakes(turn, accepting_input, undelivered).await;
                            self.record(Some(turn), || SessionRecordKind::ToolResult {
                                call: call.clone(),
                                name: bare.clone(),
                                text: result.text.clone(),
                                is_error: result.is_error,
                            });
                            if bare == "report" && !result.is_error {
                                clear_report = true;
                            }
                            self.tool_event(turn, bare).await;
                            if let Some(h) = self.session.as_ref() {
                                let _ = h.tool_result(call, result);
                            }
                        }
                        EngineEvent::JobStarted { job, command, log_path, ran_for_ms } => {
                            self.watch.engine_event(&*clock);
                            self.on_job_started(job.to_string(), command, log_path, ran_for_ms).await;
                        }
                        EngineEvent::JobFinished { job, exit_code, tail, agent_told } => {
                            self.watch.engine_event(&*clock);
                            self.on_job_finished(job.to_string(), exit_code, tail, agent_told).await;
                            self.claim_live_wakes(turn, accepting_input, undelivered).await;
                        }
                        EngineEvent::RateLimit { info } => {
                            if let Some(at) = self.on_rate_limit(&info, Some(turn)).await {
                                account_reset = Some(at);
                            }
                        }
                        EngineEvent::EndConversationCalled => {
                            end_conversation = true;
                            accepting_input = false;
                        }
                        EngineEvent::TurnEnded { outcome, usage, final_text } => {
                            let mut result = Self::ended(outcome, usage, final_text, clear_report, interrupted_by_you);
                            if end_conversation {
                                self.end_conversation(turn).await;
                            }
                            if result.reason.as_deref() == Some("account_limit")
                                && let Some(at) = account_reset
                            {
                                self.schedule_account_reset(turn, at);
                            }
                            if result.status == TurnStatus::Interrupted {
                                result.reason = Some(match self.ending {
                                    Some(Ending::Shutdown) => REASON_APP_QUIT.to_owned(),
                                    Some(Ending::Stop(_) | Ending::Failed(_)) => REASON_TASK_CLOSED.to_owned(),
                                    None => "interrupted".to_owned(),
                                });
                            }
                            return result;
                        }
                        EngineEvent::SessionClosed { reason } => {
                            self.session = None;
                            if let CloseReason::ResumeFailed { .. } = &reason
                                && !turn_started
                            {
                                return TurnResult {
                                    reason: Some(RESUME_FAILED.to_owned()),
                                    ..failed_turn("the crewmate's session couldn't be resumed")
                                };
                            }
                            if matches!(reason, CloseReason::EndConversation) {
                                self.reopen = Some("Your previous session ended.".to_owned());
                            } else {
                                self.set_agent(AgentStatus::Crashed).await;
                            }
                            return failed_turn(&format!(
                                "the crewmate's session closed: {}",
                                close_words(&reason)
                            ));
                        }
                        _ => {}
                    }
                }
                () = sleep => {
                    if interrupt_deadline.is_some_and(|d| clock.instant() >= d) {
                        // The interrupt went unanswered: the session counts as lost.
                        if let Some(h) = self.session.take() {
                            let _ = h.forget();
                        }
                        self.set_agent(AgentStatus::Crashed).await;
                        let mut result = failed_turn("the crewmate's session didn't answer the interrupt");
                        if self.ending.is_some() {
                            result.status = TurnStatus::Interrupted;
                            result.reason = Some(match self.ending {
                                Some(Ending::Shutdown) => REASON_APP_QUIT.to_owned(),
                                _ => REASON_TASK_CLOSED.to_owned(),
                            });
                        }
                        return result;
                    }
                }
            }
        }
    }

    /// `TurnStarted`: the crewmate is running, the quiet ladder starts, and the first turn moves
    /// the task `preparing → running`.
    async fn on_turn_started(&mut self, turn: TurnId) {
        let clock = self.clock();
        self.watch.turn_started(&*clock, turn);
        self.state.turn_running.store(true, Ordering::Relaxed);
        let _ = self.state.activity.send(CrewActivity::TurnStarted);
        let task = self.task;
        let agent = self.agent;
        let at = clock.now();
        let _ = self
            .svc
            .write(move |tx, _| {
                agents::set_status(tx, agent, AgentStatus::Running, at)?;
                agents::touch(tx, agent, at)?;
                if let Some(row) = task_store::load(tx, task)?
                    && row.status == TaskStatus::Preparing
                {
                    task_store::set_status(
                        tx,
                        task,
                        TaskStatus::Preparing,
                        TaskStatus::Running,
                        at,
                    )?;
                }
                Ok(())
            })
            .await;
    }

    fn ended(
        outcome: TurnOutcome,
        usage: TurnUsage,
        final_text: String,
        clear_report: bool,
        interrupted_by_you: bool,
    ) -> TurnResult {
        let (status, reason, end_kind) = match outcome {
            TurnOutcome::Completed => (TurnStatus::Completed, None, TurnEndKind::Other),
            TurnOutcome::Interrupted => (
                TurnStatus::Interrupted,
                Some("interrupted".to_owned()),
                if interrupted_by_you {
                    TurnEndKind::InterruptedByYou
                } else {
                    TurnEndKind::Other
                },
            ),
            TurnOutcome::Failed { failure } => (
                TurnStatus::Failed,
                Some(failure_word(failure.kind).to_owned()),
                if failure.kind == FailureKind::Provider {
                    TurnEndKind::FailedAtProvider
                } else {
                    TurnEndKind::Other
                },
            ),
        };
        TurnResult {
            status,
            reason,
            usage,
            final_text,
            clear_report,
            end_kind: if clear_report {
                TurnEndKind::ClearReport
            } else {
                end_kind
            },
            undelivered_wakes: Vec::new(),
        }
    }

    /// Commits a finished turn in one transaction.
    async fn commit(&self, turn: TurnId, result: &TurnResult, release_wakes: bool) {
        let agent = self.agent;
        let task = self.task;
        let at = self.clock().now();
        let status = result.status;
        let reason = result.reason.clone();
        let usage = CrewUsage {
            model_calls: result.usage.model_calls,
            input_tokens: result.usage.input_tokens,
            output_tokens: result.usage.output_tokens,
            thinking_tokens: result.usage.thinking_tokens,
            cache_read_tokens: result.usage.cache_read_tokens,
            cache_write_tokens: result.usage.cache_write_tokens,
            cost_usd: result.usage.cost_usd,
        };
        let model = self.model.clone();
        let final_text = result.final_text.clone();
        let keep = !result.clear_report;
        let undelivered = result.undelivered_wakes.clone();
        let committed = self
            .svc
            .db()
            .call(move |conn| {
                // Release only undelivered live inputs, never the original input or earlier live
                // deliveries. If release fails, don't let finish_crew_turn consume these claims.
                if !undelivered.is_empty() {
                    let tx = conn.transaction()?;
                    for id in undelivered {
                        tx.execute(
                            "UPDATE wakes SET delivered_turn_id = NULL, delivered_at = NULL
                             WHERE id = ?1 AND delivered_turn_id = ?2 AND consumed_at IS NULL",
                            rusqlite::params![id, turn],
                        )?;
                    }
                    tx.commit()?;
                }
                agents::finish_crew_turn(
                    conn,
                    &FinishedCrewTurn {
                        turn,
                        agent,
                        task,
                        status,
                        reason: reason.as_deref(),
                        usage,
                        model: model.as_ref(),
                        final_text: &final_text,
                        keep_final_text: keep,
                        release_wakes,
                        at,
                    },
                )
            })
            .await;
        if let Err(e) = committed {
            tracing::error!(%task, error = %e, "couldn't record the crewmate's finished turn");
        }
        self.svc.changed();
    }

    /// After the commit: the crewmate's status and the idle ladder.
    async fn after_turn(&mut self, turn: TurnId, result: &TurnResult) {
        self.stale_signs();
        self.state.turn_running.store(false, Ordering::Relaxed);
        let _ = self.state.activity.send(CrewActivity::TurnEnded);
        self.record(Some(turn), || SessionRecordKind::TurnEnded {
            status: result.status,
        });
        if self.ending.is_some() {
            self.watch.clear();
            return;
        }
        let task = self.task;
        let status = self
            .svc
            .db()
            .call(move |conn| task_store::load(conn, task))
            .await
            .ok()
            .flatten()
            .map(|r| r.status);
        let waiting = status == Some(TaskStatus::Waiting);
        if self.session.is_some() || !matches!(status, None | Some(TaskStatus::Closed)) {
            self.set_agent(if waiting {
                AgentStatus::Waiting
            } else {
                AgentStatus::Idle
            })
            .await;
        }
        let how = if waiting && result.end_kind != TurnEndKind::ClearReport {
            TurnEndKind::Waiting
        } else {
            result.end_kind
        };
        // A task that reported, waits, is held or stuck has nobody to nudge.
        let ladder_applies = matches!(status, Some(TaskStatus::Running));
        let clock = self.clock();
        self.watch.turn_ended(
            &*clock,
            turn,
            if ladder_applies {
                how
            } else {
                TurnEndKind::ClearReport
            },
        );
    }

    async fn end_conversation(&mut self, turn: TurnId) {
        let task = self.task;
        let agent = self.agent;
        let at = self.clock().now();
        let _ = self
            .svc
            .write(move |tx, _| {
                qm_store::events::insert(
                    tx,
                    &qm_store::events::NewEvent {
                        kind: END_CONVERSATION_EVENT,
                        agent: Some(agent),
                        task: Some(task),
                        turn: Some(turn),
                        summary: &format!("{task}'s crewmate ended its conversation; a new session opens next turn"),
                        payload: serde_json::json!({}),
                        at,
                    },
                )
                .map(drop)
            })
            .await;
        self.reopen = Some("Your previous session ended.".to_owned());
    }

    /// Dispatches one of our tools with the crewmate's caller.
    async fn dispatch(
        &self,
        turn: TurnId,
        started_by: StartedBy,
        name: &str,
        args: serde_json::Value,
    ) -> ToolResult {
        let Some(host) = self.svc.host() else {
            return ToolResult::error("error: the app's tools aren't available right now");
        };
        // While a helper agent of the crewmate runs, only its read-only tools work; a helper can
        // call them as the crewmate, and neither acts for the task until it's done.
        let changes_state =
            qm_tools::registry::find(Role::Crewmate, self.plan, name).is_some_and(|d| {
                d.class != qm_types::rules::ToolClass::Read
                    && d.scope == qm_tools::EngineScope::Both
            });
        if changes_state {
            let task = self.task;
            let tool = name.to_owned();
            match self
                .svc
                .db()
                .call(move |conn| {
                    crate::jobs::helpers_running(
                        conn,
                        task,
                        crate::jobs::HelperWait::Crewmate(&tool),
                    )
                })
                .await
            {
                Ok(Some(why)) => {
                    return ToolResult::error(
                        qm_tools::handler::ToolFailure::Refused(qm_types::error::Refusal::new(why))
                            .render(),
                    );
                }
                Ok(None) => {}
                Err(e) => return ToolResult::error(format!("error: {e}")),
            }
        }
        let caller = Caller {
            agent: self.agent,
            role: Role::Crewmate,
            turn: Some(TurnCx { turn, started_by }),
            task: Some(self.task),
            secondmate: self.secondmate,
            plan: self.plan,
        };
        let clock = self.clock();
        let cx = ToolCx {
            caller: &caller,
            clock: &*clock,
            host: &*host,
        };
        self.svc
            .deps()
            .dispatcher
            .dispatch(&cx, name, args)
            .await
            .result
    }

    /// Our tools the session doesn't offer, bare or prefixed.
    async fn check_tools(&self, offered: &[String]) {
        let missing: Vec<String> = qm_tools::registry::descriptors(Role::Crewmate, self.plan)
            .into_iter()
            .map(|d| d.name)
            .filter(|name| {
                !offered.iter().any(|t| {
                    t == name || t.strip_prefix(MCP_PREFIX).is_some_and(|bare| bare == name)
                })
            })
            .collect();
        if missing.is_empty() {
            return;
        }
        tracing::warn!(task = %self.task, ?missing, "the crewmate's session doesn't offer every crewmate tool");
        let task = self.task;
        let agent = self.agent;
        let at = self.clock().now();
        let _ = self
            .svc
            .write(move |tx, _| {
                task_store::event(
                    tx,
                    TOOLS_MISSING_EVENT,
                    task,
                    Some(agent),
                    &format!("{task}'s session is missing tools: {}", missing.join(", ")),
                    serde_json::json!({ "missing": missing }),
                    at,
                )
                .map(drop)
            })
            .await;
    }

    /// A `task.tool` event: the task pane's activity line ("▸ Bash cargo test auth").
    /// Adds a finished model call's tokens to the running turn, so the task pane shows them live.
    async fn call_tokens(&self, turn: TurnId, usage: &qm_engine::CallUsage) {
        let tokens = qm_store::turns::CallTokens {
            input: usage.input_tokens,
            output: usage.output_tokens,
            thinking: usage.thinking_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
        };
        if let Err(e) = self
            .svc
            .write(move |tx, _| qm_store::turns::add_call_tokens(tx, turn, &tokens))
            .await
        {
            tracing::warn!(error = %e, "couldn't add a model call's tokens");
        }
    }

    async fn tool_event(&self, turn: TurnId, line: String) {
        let _ = self.state.activity.send(CrewActivity::Tool(line.clone()));
        let task = self.task;
        let agent = self.agent;
        let at = self.clock().now();
        let _ = self
            .svc
            .write(move |tx, _| {
                qm_store::events::insert(
                    tx,
                    &qm_store::events::NewEvent {
                        kind: TOOL_EVENT,
                        agent: Some(agent),
                        task: Some(task),
                        turn: Some(turn),
                        summary: &line,
                        payload: serde_json::json!({}),
                        at,
                    },
                )
                .map(drop)
            })
            .await;
    }

    /// Claims supervisor steering and job exits into an accepted, uninterrupted turn. Ordinary
    /// wakes keep the normal next-turn debounce, and live input keeps its supervisor markers.
    async fn claim_live_wakes(
        &mut self,
        turn: TurnId,
        accepting_input: bool,
        undelivered: &mut Vec<WakeId>,
    ) {
        if !accepting_input
            || self.ending.is_some()
            || *self.state.cancel.borrow()
            || self.session.is_none()
        {
            return;
        }
        let agent = self.agent;
        let at = self.clock().now();
        let (attempted_tx, attempted_rx) = oneshot::channel();
        let claimed = self
            .svc
            .db()
            .call(move |conn| {
                let ids =
                    inbox_store::pending_ids(conn, agent, &[WakeKind::Steer, WakeKind::JobExited])?;
                let _ = attempted_tx.send(ids.clone());
                inbox_store::claim_ids(conn, &ids, turn, at)
            })
            .await;
        match claimed {
            Ok(claimed) if !claimed.is_empty() => {
                let text = render_wakes(&claimed, &self.supervisor_label);
                // Stop/quit can latch cancellation while the database claim is in flight.
                if !*self.state.cancel.borrow()
                    && self
                        .session
                        .as_ref()
                        .is_some_and(|h| h.inject(TurnInput::text(text)).is_ok())
                {
                    self.record_wakes(turn, &claimed);
                } else {
                    undelivered.extend(claimed.iter().map(|wake| wake.id));
                    self.wakes.note(self.clock().instant());
                    tracing::warn!(%turn, "the crewmate's live wakes weren't sent; they will be retried");
                }
            }
            Ok(_) => {}
            Err(error) => {
                // A mirror flush can fail after the memory claim commits. Retain just this
                // attempt's pending IDs; release is guarded by this turn and unconsumed state.
                undelivered.extend(attempted_rx.await.unwrap_or_default());
                self.wakes.note(self.clock().instant());
                tracing::warn!(%turn, %error, "couldn't claim the crewmate's live wakes");
            }
        }
        // Catch up with unrelated durable wakes too, but don't reset their existing window
        // whenever another tool completes (or invent a window when nothing is pending).
        if self.wakes.ready_at().is_none() {
            self.check_pending().await;
        }
    }

    /// Your message, injected unmarked into the running turn.
    async fn inject_message(&mut self, message: MessageId) {
        let text = self
            .svc
            .db()
            .call(move |conn| qm_store::messages::text(conn, message))
            .await
            .ok()
            .flatten();
        match (text, self.session.as_ref()) {
            (Some(text), Some(h)) => {
                self.record_message(None, message, &text);
                let _ = h.inject(TurnInput {
                    blocks: vec![InputBlock::Text {
                        text: qm_policy::markers::neutralise(&text),
                    }],
                });
            }
            _ => self.messages.push_back(message),
        }
    }

    async fn on_job_started(
        &mut self,
        label: String,
        command: String,
        log_path: Option<std::path::PathBuf>,
        ran_for_ms: Option<u64>,
    ) {
        self.state.jobs_running.fetch_add(1, Ordering::Relaxed);
        let started = crate::jobs::JobStart {
            label,
            command,
            log_path,
            ran_for_ms,
        };
        if let Err(e) = crate::jobs::job_started(&self.svc, self.task, self.agent, started).await {
            tracing::warn!(task = %self.task, error = %e, "couldn't record a started job");
        }
    }

    /// A job finished: recorded with its wake (none when `agent_told`). The caller inside a
    /// running turn claims live wakes only while that turn still accepts input.
    async fn on_job_finished(
        &mut self,
        label: String,
        exit_code: Option<i32>,
        tail: String,
        agent_told: bool,
    ) {
        saturating_decrement(&self.state.jobs_running);
        let claude = self
            .routed
            .as_ref()
            .is_some_and(|(r, _)| r.engine == EngineKind::Claude);
        let finished = crate::jobs::JobEnd {
            label,
            exit_code,
            tail,
            claude,
            agent_told,
        };
        if let Err(e) = crate::jobs::job_finished(&self.svc, self.task, self.agent, finished).await
        {
            tracing::warn!(task = %self.task, error = %e, "couldn't record a finished job");
        }
    }

    /// The account hit its limit: a sign of life until it resets. Returns the reset time.
    async fn on_rate_limit(
        &mut self,
        info: &qm_engine::RateLimitInfo,
        turn: Option<TurnId>,
    ) -> Option<Timestamp> {
        if info.status != RateLimitStatus::Rejected {
            return None;
        }
        let clock = self.clock();
        let resets_at = info.resets_at;
        if let Some(at) = resets_at {
            let left: Duration = clock
                .now()
                .duration_until(at)
                .try_into()
                .unwrap_or(Duration::ZERO);
            self.cut_off_until = Some(clock.instant() + left);
        }
        let task = self.task;
        let agent = self.agent;
        let now = clock.now();
        let summary = match resets_at {
            Some(at) => format!("{task}'s account is at its limit until {at}"),
            None => format!("{task}'s account is at its limit"),
        };
        let _ = self
            .svc
            .write(move |tx, _| {
                task_store::event(
                    tx,
                    ACCOUNT_LIMIT_EVENT,
                    task,
                    Some(agent),
                    &summary,
                    serde_json::json!({ "resets_at": resets_at, "turn": turn }),
                    now,
                )
                .map(drop)
            })
            .await;
        resets_at
    }

    /// Posts an `account_reset` wake at `at` through the scheduler.
    fn schedule_account_reset(&self, turn: TurnId, at: Timestamp) {
        let svc = self.svc.clone();
        let task = self.task;
        let agent = self.agent;
        self.svc.deps().scheduler.at_time(
            format!("account_reset:{task}"),
            at,
            Box::new(move || {
                Box::pin(async move {
                    let now = svc.clock().now();
                    let _ = svc
                        .write(move |tx, notify| {
                            let wake = new_wake(
                                agent,
                                WakeKind::AccountReset,
                                task,
                                format!("account_reset:{task}:{turn}"),
                                "Your account's usage limit has reset. Carry on.",
                                "account reset",
                                serde_json::json!({}),
                                now,
                            );
                            Inbox::post(tx, &wake, notify).map(drop)
                        })
                        .await;
                })
            }),
        );
    }
}

/// What starts a turn.
#[derive(Clone, Copy, Debug)]
enum NextTurn {
    User(MessageId),
    App,
}

/// The reason a turn whose input never arrived carries, internally.
const RESUME_FAILED: &str = "resume_failed";

fn failed_turn(reason: &str) -> TurnResult {
    TurnResult {
        status: TurnStatus::Failed,
        reason: Some(reason.to_owned()),
        usage: TurnUsage::default(),
        final_text: String::new(),
        clear_report: false,
        end_kind: TurnEndKind::Other,
        undelivered_wakes: Vec::new(),
    }
}

const fn failure_word(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Provider => "provider",
        FailureKind::AccountLimit => "account_limit",
        FailureKind::ProcessExited => "process_exited",
        FailureKind::Protocol => "protocol",
        FailureKind::Internal => "internal",
    }
}

fn close_words(reason: &CloseReason) -> String {
    match reason {
        CloseReason::Requested => "it was closed".to_owned(),
        CloseReason::EndConversation => "the crewmate ended its conversation".to_owned(),
        CloseReason::ResumeFailed { message } => format!("it couldn't be resumed ({message})"),
        CloseReason::Crashed {
            exit_code, signal, ..
        } => match (exit_code, signal) {
            (Some(code), _) => format!("its process exited with code {code}"),
            (None, Some(signal)) => format!("its process was killed by signal {signal}"),
            (None, None) => "its process died".to_owned(),
        },
    }
}

async fn next_event(handle: Option<&mut SessionHandle>) -> Option<EngineEvent> {
    match handle {
        Some(h) => h.next_event().await,
        None => std::future::pending().await,
    }
}

/// Stops every job or setup group still recorded open for the task, so nothing outlives it:
/// SIGTERM, a short grace, then SIGKILL. A group is signalled only while its leader is the recorded
/// process (same start time), never a reused process group ID.
pub(crate) async fn stop_open_groups(svc: &TaskService, task: TaskId, agent: Option<AgentId>) {
    let groups = svc
        .db()
        .call(move |conn| job_store::open_groups_of_task(conn, task, agent))
        .await
        .unwrap_or_default();
    if groups.is_empty() {
        return;
    }
    let sup = Arc::clone(&svc.deps().supervisor);
    let clock = svc.clock_arc();
    let ours = |group: &job_store::OpenGroup| {
        let g = ProcessGroup::new(group.pgid);
        sup.group_members(g).iter().any(|m| {
            m.pid == group.pid && m.started_at.as_millisecond() == group.started_at.as_millisecond()
        })
    };
    let live: Vec<&job_store::OpenGroup> = groups.iter().filter(|g| ours(g)).collect();
    if live.is_empty() {
        return;
    }
    for group in &live {
        let _ = sup.signal_group(ProcessGroup::new(group.pgid), Signal::Term);
    }
    clock.sleep(GROUP_TERM_GRACE).await;
    for group in &live {
        let g = ProcessGroup::new(group.pgid);
        if sup.group_alive(g) {
            let _ = sup.signal_group(g, Signal::Kill);
        }
    }
}

/// How long a recorded group gets between SIGTERM and SIGKILL.
const GROUP_TERM_GRACE: Duration = Duration::from_secs(3);

/// Takes one from `n`, never below zero. A plain compare-exchange loop: `fetch_update` is
/// deprecated from Rust 1.99 and its replacement, `try_update`, doesn't exist before it.
fn saturating_decrement(n: &AtomicU32) {
    let mut current = n.load(Ordering::Relaxed);
    while current > 0 {
        match n.compare_exchange_weak(current, current - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

/// The note a planner's fresh session opens with after its project's autonomy changed.
fn mode_changed_note(mode: PlanMode) -> String {
    match mode {
        PlanMode::Internal => "The project's autonomy changed to auto, so you're in a fresh session \
             in the new mode. Your plan so far is in $QM_PLAN_DIR/plan.md: continue from it, decide \
             every open point yourself, and record each choice under \"Decisions made without the \
             user\"."
            .to_owned(),
        PlanMode::Interactive => "The project's autonomy changed to normal, so you're in a fresh \
             session in the new mode. Your plan so far is in $QM_PLAN_DIR/plan.md: continue from \
             it, and turn the decisions you made alone, and anything not crystal clear, into \
             questions for the user."
            .to_owned(),
    }
}

#[cfg(test)]
mod forgetting_tests {
    use super::*;
    use qm_engine::{SessionCommand, session_channel};

    /// The command a runner ending with `ending` sends its live session, answered with
    /// `SessionClosed`.
    async fn the_command_finish_sends(ending: Option<Ending>) -> Option<SessionCommand> {
        let fixture = crate::turns::forget_tests::Fixture::new().await;
        let (svc, scheduler) = fixture.task_service();
        let (_tx, rx) = mpsc::unbounded_channel();
        let (activity, _) = broadcast::channel(8);
        // No task row: the asynchronous retirement exits without touching a repository or home.
        let task = TaskId::new(999).unwrap();
        let mut runner = CrewRunner::new(
            task,
            AgentId::new(2),
            svc.clone(),
            rx,
            Arc::new(RunnerState::new(activity)),
            RunnerStart::Resume,
        );
        let (handle, mut side) = session_channel();
        runner.session = Some(handle);
        runner.ending = ending;
        let engine = async {
            let command = side.commands.recv().await;
            side.events
                .send(EngineEvent::SessionClosed {
                    reason: CloseReason::Requested,
                })
                .await
                .unwrap();
            command
        };
        let ((), command) = tokio::join!(runner.finish(), engine);
        // A barrier lets the spawned retirement finish its read of the absent task.
        svc.db().call(|_| Ok(())).await.unwrap();
        drop(svc);
        scheduler.abort();
        let _ = scheduler.await;
        command
    }

    #[tokio::test]
    async fn failed_runner_forgets_before_unregistering_and_retiring() {
        assert_eq!(
            the_command_finish_sends(Some(Ending::Failed(
                "counterexample: failed with a live provider session".into(),
            )))
            .await,
            Some(SessionCommand::Forget),
            "failure must not issue resumable Close and then unregister the only handle"
        );
    }

    #[tokio::test]
    async fn quitting_closes_the_crewmates_session_so_it_resumes_at_the_next_start() {
        assert_eq!(
            the_command_finish_sends(Some(Ending::Shutdown)).await,
            Some(SessionCommand::Close)
        );
    }
}

#[cfg(test)]
mod live_wake_tests;

#[cfg(test)]
mod move_tests;

#[cfg(test)]
mod job_wake_tests {
    use super::*;
    use qm_engine::{EngineSide, SessionCommand, session_channel};
    use qm_types::ids::JobLabel;

    const CREWMATE: i64 = 3;

    /// A crewmate runner for the fixture's open task 2, on a session whose engine side the test
    /// plays.
    struct Crew {
        fixture: crate::turns::forget_tests::Fixture,
        runner: CrewRunner,
        side: EngineSide,
        // Keep the input channel open: dropping it means Shutdown, not an idle test producer.
        _tx: mpsc::UnboundedSender<CrewInput>,
        scheduler: tokio::task::JoinHandle<()>,
    }

    impl Crew {
        async fn new() -> Self {
            let fixture = crate::turns::forget_tests::Fixture::new().await;
            fixture
                .db()
                .call(|conn| {
                    conn.execute_batch(&format!(
                        "INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                         VALUES ({CREWMATE}, 'crewmate', 1, 2, '/c3', 'idle', 1, 5);"
                    ))?;
                    qm_store::seq::observe_all(conn)?;
                    Ok(())
                })
                .await
                .unwrap();
            let (svc, scheduler) = fixture.task_service();
            let (tx, rx) = mpsc::unbounded_channel();
            let (activity, _) = broadcast::channel(64);
            let mut runner = CrewRunner::new(
                TaskId::new(2).unwrap(),
                AgentId::new(CREWMATE),
                svc,
                rx,
                Arc::new(RunnerState::new(activity)),
                RunnerStart::Resume,
            );
            let (handle, side) = session_channel();
            runner.session = Some(handle);
            runner
                .on_idle_event(Some(EngineEvent::JobStarted {
                    job: job(),
                    command: "FAST helper".into(),
                    log_path: None,
                    ran_for_ms: None,
                }))
                .await;
            Self {
                fixture,
                runner,
                side,
                _tx: tx,
                scheduler,
            }
        }

        async fn count(&self, sql: &'static str) -> i64 {
            self.fixture
                .db()
                .call(move |conn| Ok(conn.query_row(sql, [], |r| r.get(0))?))
                .await
                .unwrap()
        }

        async fn job_wakes(&self) -> i64 {
            self.count(
                "SELECT count(*) FROM wakes WHERE target_agent_id = 3 AND kind = 'job_exited'",
            )
            .await
        }

        async fn finished_jobs(&self) -> i64 {
            self.count(
                "SELECT count(*) FROM jobs WHERE agent_id = 3 AND exited_at IS NOT NULL AND exit_code = 0",
            )
            .await
        }

        async fn job_events(&self) -> i64 {
            self.count("SELECT count(*) FROM events WHERE kind = 'task.job_finished'")
                .await
        }

        /// The refusal of the crewmate's `report` while its helpers run.
        async fn helpers_refusal(&self) -> String {
            self.fixture
                .db()
                .call(|conn| {
                    crate::jobs::helpers_running(
                        conn,
                        TaskId::new(2).unwrap(),
                        crate::jobs::HelperWait::Crewmate("report"),
                    )
                })
                .await
                .unwrap()
                .expect("a helper runs")
        }

        /// Plays a turn the engine started by itself, with `events` before its end.
        async fn adopted_turn(&mut self, events: Vec<EngineEvent>) {
            for event in events.into_iter().chain([EngineEvent::TurnEnded {
                outcome: TurnOutcome::Completed,
                usage: TurnUsage::default(),
                final_text: "done".into(),
            }]) {
                self.side.events.send(event).await.unwrap();
            }
            self.runner
                .on_idle_event(Some(EngineEvent::TurnStarted))
                .await;
        }

        /// The inputs the runner sent the engine.
        fn sent_inputs(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(command) = self.side.commands.try_recv() {
                if let SessionCommand::Inject { input } | SessionCommand::StartTurn { input } =
                    command
                {
                    out.push(format!("{input:?}"));
                }
            }
            out
        }

        async fn done(self) {
            drop(self.runner);
            self.scheduler.abort();
            let _ = self.scheduler.await;
        }
    }

    fn job() -> JobLabel {
        JobLabel::helper("FAST").unwrap()
    }

    fn finished(agent_told: bool) -> EngineEvent {
        EngineEvent::JobFinished {
            job: job(),
            exit_code: Some(0),
            tail: "FAST-done".into(),
            agent_told,
        }
    }

    /// Claude reported the job to the model and started a turn for it: the job's end is
    /// recorded, the turn is one app-event turn, and the app posts no wake of its own.
    #[tokio::test]
    async fn a_job_the_agent_was_told_of_posts_no_wake_and_its_turn_is_adopted() {
        let mut crew = Crew::new().await;
        crew.runner.on_idle_event(Some(finished(true))).await;
        crew.adopted_turn(vec![EngineEvent::TextLine {
            text: "FAST is done.".into(),
        }])
        .await;

        assert_eq!(crew.finished_jobs().await, 1);
        assert_eq!(crew.job_events().await, 1);
        assert_eq!(crew.job_wakes().await, 0);
        let turns = crew
            .count("SELECT count(*) FROM turns WHERE agent_id = 3 AND started_by = 'app_event'")
            .await;
        let all = crew
            .count("SELECT count(*) FROM turns WHERE agent_id = 3")
            .await;
        assert_eq!((turns, all), (1, 1));
        assert_eq!(crew.sent_inputs(), Vec::<String>::new());
        crew.done().await;
    }

    /// The same job ending inside a turn: nothing is injected.
    #[tokio::test]
    async fn a_job_the_agent_was_told_of_mid_turn_injects_nothing() {
        let mut crew = Crew::new().await;
        crew.adopted_turn(vec![finished(true)]).await;
        assert_eq!(crew.finished_jobs().await, 1);
        assert_eq!(crew.job_wakes().await, 0);
        assert_eq!(crew.sent_inputs(), Vec::<String>::new());
        crew.done().await;
    }

    /// A job the agent never heard of (its process died, or the provider engine) still wakes it:
    /// into the running turn here.
    #[tokio::test]
    async fn a_job_the_agent_was_not_told_of_is_injected() {
        let mut crew = Crew::new().await;
        crew.adopted_turn(vec![finished(false)]).await;
        assert_eq!(crew.finished_jobs().await, 1);
        assert_eq!(crew.job_wakes().await, 1);
        let inputs = crew.sent_inputs();
        assert!(
            inputs.len() == 1 && inputs[0].contains("job_exited"),
            "{inputs:?}"
        );
        crew.done().await;
    }

    /// The crewmate's refusal while helpers run counts them in good English.
    #[tokio::test]
    async fn the_helpers_refusal_agrees_with_its_count() {
        let mut crew = Crew::new().await;
        let one = crew.helpers_refusal().await;
        assert!(
            one.starts_with("a helper agent of yours is still running (FAST helper)"),
            "{one}"
        );
        crew.runner
            .on_idle_event(Some(EngineEvent::JobStarted {
                job: JobLabel::helper("SLOW").unwrap(),
                command: "SLOW helper".into(),
                log_path: None,
                ran_for_ms: None,
            }))
            .await;
        let two = crew.helpers_refusal().await;
        assert!(
            two.starts_with("2 helper agents of yours are still running"),
            "{two}"
        );
        crew.done().await;
    }

    /// Between turns, such a job posts its wake for the next turn.
    #[tokio::test]
    async fn a_job_the_agent_was_not_told_of_posts_a_wake_between_turns() {
        let mut crew = Crew::new().await;
        crew.runner.on_idle_event(Some(finished(false))).await;
        assert_eq!(crew.finished_jobs().await, 1);
        assert_eq!(crew.job_wakes().await, 1);
        crew.done().await;
    }
}
