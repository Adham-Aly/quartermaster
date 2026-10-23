//! One provider session: a Tokio task driven by `SessionCommand`s.
//!
//! Between turns the task waits on commands and the built-in tools' job notices. A turn
//! ([`crate::agent_loop`]) runs inside the same task and keeps serving commands while it waits on
//! the network or on tools: `Inject` goes to the steering queue (delivered before the next model
//! call), `ToolResult` resolves a pending call, `Interrupt` cancels the turn, `SetModel` waits for
//! the next idle moment (and writes a `model_change` entry), `Close` cancels the turn and closes,
//! and a `StartTurn` that arrives mid-turn is queued (the core shouldn't send one). Job
//! notices become `JobStarted` (inside the turn, before that tool's result) and `JobFinished`
//! (any time), exactly one each.
//!
//! **Crash containment.** [`supervise`] runs the session in its own task: if it panics, the
//! supervisor ends an open turn `Failed { Internal }`, stops the built-in tools' jobs and reports
//! them finished, and closes the session `Crashed`. Other sessions never notice.

use crate::adapters::{self, WireApi};

#[cfg(test)]
mod forget_tests;
use crate::agent_loop::CompactError;
use crate::builtin::{BuiltinTools, JobNotice};
use crate::compaction::Context;
use crate::engine::{Shared, adapter_capability, api_for};
use crate::limits::Limits;
use crate::model::ModelProfile;
use crate::retry::RetryPolicy;
use crate::session_store::{self, Entry, SessionStore, SessionStoreError};
use crate::transcript::Message;
use qm_engine::{
    ACTIVITY_EVERY, ActivityGate, Builtins, CloseReason, ContextBasis, EngineEvent, EngineSide,
    FailureKind, InitReport, RateLimitStatus, SessionCommand, SessionSpec, SystemPrompt,
    ToolDescriptor, ToolResult, TurnFailure, TurnInput, TurnOutcome, TurnUsage,
};
use qm_types::ids::{JobLabel, ModelId, SessionRef};
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long `Close` waits for stopped jobs to report before reporting them itself.
const JOB_SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

/// What the crash supervisor needs to know about a running session.
#[derive(Debug, Default)]
pub(crate) struct Status {
    pub(crate) turn_open: bool,
    pub(crate) running_jobs: BTreeSet<JobLabel>,
    pub(crate) closed: bool,
}

/// Why a wait stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cancel {
    /// `Interrupt`: end the turn, keep the session.
    Interrupt,
    /// `Close` (or the handle went away): end the turn, then close.
    Close,
}

/// The session's state.
pub(crate) struct Session {
    pub(crate) shared: Arc<Shared>,
    pub(crate) spec: SessionSpec,
    /// The session ID (a UUID v7): the file's name and the prompt-cache key.
    pub(crate) id: String,
    pub(crate) system_prompt: String,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    events: mpsc::Sender<EngineEvent>,
    status: Arc<Mutex<Status>>,
    pub(crate) gate: ActivityGate,
    pub(crate) model: ModelProfile,
    pending_model: Option<ModelId>,
    pub(crate) limits: Limits,
    pub(crate) retry: RetryPolicy,
    /// Every tool offered: ours, then the built-in ones.
    pub(crate) tools: Vec<ToolDescriptor>,
    /// Our tools' names (their calls go to the core).
    pub(crate) our_tools: HashSet<String>,
    pub(crate) builtins: Option<Arc<dyn BuiltinTools>>,
    job_notices: Option<mpsc::UnboundedReceiver<JobNotice>>,
    jobs: Jobs,
    pub(crate) context: Context,
    /// The size the automatic compaction's repeat guard waits to grow past: the last compaction's
    /// `tokens_after`, or the size at a failed automatic attempt (see
    /// [`crate::compaction::auto_compaction_due`]).
    pub(crate) compaction_guard: Option<u64>,
    pub(crate) history: SessionStore,
    /// Turns and compactions that wait for the running turn to end, in order.
    queued_turns: VecDeque<Queued>,
    pub(crate) steering: Vec<TurnInput>,
    /// Results the core sent for our pending tool calls, by call ID.
    pub(crate) pending_results: HashMap<String, Option<ToolResult>>,
    close_requested: bool,
    /// The session ends for good: its transcript goes with it.
    forget_requested: bool,
    pub(crate) in_turn: bool,
    /// The last rate-limit status reported per window, so only changes are reported.
    pub(crate) rate_limits: HashMap<String, RateLimitStatus>,
}

/// What waits to run after the current turn.
#[derive(Debug)]
enum Queued {
    Turn(TurnInput),
    /// A compaction you asked for (`SessionCommand::Compact`).
    Compact,
}

/// Job bookkeeping, so each job gets exactly one `JobStarted` and one `JobFinished`.
#[derive(Debug, Default)]
struct Jobs {
    /// Reported started and not yet finished.
    running: BTreeSet<JobLabel>,
    /// Ever reported started.
    started: HashSet<JobLabel>,
    /// Finished before their start was reported (the notice raced the call's outcome).
    early_finished: HashMap<JobLabel, (Option<i32>, String)>,
    /// Started outside a turn, so never reported: their end isn't reported either.
    suppressed: HashSet<JobLabel>,
}

/// Runs one session under a crash supervisor (see the module docs).
pub(crate) async fn supervise(
    shared: Arc<Shared>,
    spec: SessionSpec,
    side: EngineSide,
    builtins: Option<Arc<dyn BuiltinTools>>,
    seed: u64,
) {
    let status = Arc::new(Mutex::new(Status::default()));
    let events = side.events.clone();
    let inner = tokio::spawn(run(
        Arc::clone(&shared),
        spec,
        side,
        builtins.clone(),
        seed,
        Arc::clone(&status),
    ));
    let Err(join_error) = inner.await else {
        return;
    };
    let message = if join_error.is_panic() {
        panic_message(&*join_error.into_panic())
    } else {
        "the session task was cancelled".to_owned()
    };
    tracing::error!(error = %message, "a provider session failed; closing it");
    let (turn_open, running, closed) = {
        let s = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (s.turn_open, s.running_jobs.clone(), s.closed)
    };
    if closed {
        return;
    }
    if turn_open {
        let _ = events
            .send(EngineEvent::TurnEnded {
                outcome: TurnOutcome::Failed {
                    failure: TurnFailure {
                        kind: FailureKind::Internal,
                        message: format!("the provider engine failed: {message}"),
                    },
                },
                usage: TurnUsage::default(),
                final_text: String::new(),
            })
            .await;
    }
    if let Some(tools) = &builtins {
        tools.shutdown().await;
    }
    for job in running {
        let _ = events
            .send(EngineEvent::JobFinished {
                job,
                exit_code: None,
                tail: String::new(),
            })
            .await;
    }
    let _ = events
        .send(EngineEvent::SessionClosed {
            reason: CloseReason::Crashed {
                exit_code: None,
                signal: None,
                stderr_tail: message,
            },
        })
        .await;
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// Starts (or resumes) the session, then serves it until it closes.
async fn run(
    shared: Arc<Shared>,
    spec: SessionSpec,
    side: EngineSide,
    builtins: Option<Arc<dyn BuiltinTools>>,
    seed: u64,
    status: Arc<Mutex<Status>>,
) {
    let EngineSide { commands, events } = side;
    let session = match Session::start(
        shared,
        spec,
        commands,
        events.clone(),
        builtins,
        seed,
        status,
    ) {
        Ok(session) => session,
        Err(reason) => {
            let _ = events.send(EngineEvent::SessionClosed { reason }).await;
            return;
        }
    };
    session.serve().await;
}

impl Drop for Session {
    fn drop(&mut self) {
        // A panic/aborted task cannot receive a later Forget. Only an orderly Close is
        // resumable (for example an effort change); terminal and abandoned sessions are not.
        let closed = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed;
        if self.forget_requested || !closed {
            SessionStore::forget(&self.id);
        }
    }
}

impl Session {
    #[allow(clippy::too_many_arguments)] // The parts of a session, each needed once.
    fn start(
        shared: Arc<Shared>,
        spec: SessionSpec,
        commands: mpsc::UnboundedReceiver<SessionCommand>,
        events: mpsc::Sender<EngineEvent>,
        builtins: Option<Arc<dyn BuiltinTools>>,
        seed: u64,
        status: Arc<Mutex<Status>>,
    ) -> Result<Self, CloseReason> {
        let now = shared.clock.now();
        let model = shared.models.profile(spec.account.provider, &spec.model);
        let (session_id, history, context) = if let Some(reference) = &spec.resume {
            let loaded =
                SessionStore::resume(reference.as_str()).map_err(|e: SessionStoreError| {
                    CloseReason::ResumeFailed {
                        message: e.to_string(),
                    }
                })?;
            // The core passes the model to use; a model change in the history is history.
            let (context, _model) = session_store::rebuild_context(&loaded.entries);
            (reference.as_str().to_owned(), loaded.store, context)
        } else {
            let id = uuid::Uuid::now_v7().hyphenated().to_string();
            let history = SessionStore::create(
                &id,
                now,
                spec.account.provider,
                spec.model.clone(),
                spec.effort,
            );
            (id, history, Context::default())
        };
        let mut system_prompt = match &spec.prompt {
            SystemPrompt::Replace { text } | SystemPrompt::AppendToClaudeCode { text } => {
                text.clone()
            }
        };
        if let Builtins::ProviderFileTools {
            scratchpad: Some(dir),
            ..
        } = &spec.builtins
        {
            system_prompt.push_str(&crate::files::scratchpad_section(dir));
        }
        let mut tools = spec.tools.clone();
        let our_tools = spec.tools.iter().map(|t| t.name.clone()).collect();
        if let Some(b) = &builtins {
            tools.extend(b.descriptors());
        }
        let job_notices = builtins.as_ref().and_then(|b| b.take_job_notices());
        let limits = Limits::from_spec(spec.limits);
        Ok(Self {
            shared,
            id: session_id,
            system_prompt,
            commands,
            events,
            status,
            gate: ActivityGate::new(),
            model,
            pending_model: None,
            limits,
            retry: RetryPolicy::new(limits.max_retries, seed),
            tools,
            our_tools,
            builtins,
            job_notices,
            jobs: Jobs::default(),
            context,
            compaction_guard: None,
            history,
            queued_turns: VecDeque::new(),
            steering: Vec::new(),
            pending_results: HashMap::new(),
            close_requested: false,
            forget_requested: false,
            in_turn: false,
            rate_limits: HashMap::new(),
            spec,
        })
    }

    /// The init report (never the prompt or a secret).
    fn init_report(&self) -> InitReport {
        let api: Option<WireApi> = api_for(
            self.spec.account.provider,
            self.shared.credential_kind(&self.spec.account).as_ref(),
        );
        let tool_names: Vec<String> = self.tools.iter().map(|t| t.name.clone()).collect();
        InitReport {
            engine_version: Some(format!(
                "quartermaster-provider/{}",
                env!("CARGO_PKG_VERSION")
            )),
            model: Some(self.spec.model.clone()),
            tools: tool_names.clone(),
            permission_mode: None,
            capabilities: api
                .map(|a| vec![adapter_capability(a).to_owned()])
                .unwrap_or_default(),
            raw: json!({
                "provider": self.spec.account.provider,
                "api": api.map(WireApi::as_str),
                "base_url": self.spec.account.base_url,
                "model": self.spec.model,
                "effort": self.spec.effort,
                "tools": tool_names,
                "prompt_chars": self.system_prompt.chars().count(),
            }),
        }
    }

    async fn serve(mut self) {
        let Ok(session) = SessionRef::new(self.id.clone()) else {
            return;
        };
        let init = self.init_report();
        self.emit(EngineEvent::SessionReady {
            session,
            resumed: self.spec.resume.is_some(),
            init,
        })
        .await;
        // Passive: the point and window are known; the size is unknown until the first call.
        let reading = self.context_reading(None, ContextBasis::SessionStart);
        self.emit(EngineEvent::ContextObserved { reading }).await;
        loop {
            if self.close_requested {
                self.close().await;
                return;
            }
            match self.queued_turns.pop_front() {
                Some(Queued::Turn(input)) => {
                    self.run_turn(input).await;
                    self.after_turn().await;
                    continue;
                }
                Some(Queued::Compact) => {
                    self.run_compaction().await;
                    self.after_turn().await;
                    continue;
                }
                None => {}
            }
            tokio::select! {
                command = self.commands.recv() => self.idle_command(command).await,
                notice = recv_notice(&mut self.job_notices) => self.on_notice(notice).await,
            }
        }
    }

    /// A command between turns.
    async fn idle_command(&mut self, command: Option<SessionCommand>) {
        match command {
            None | Some(SessionCommand::Forget) => {
                self.forget_requested = true;
                self.close_requested = true;
            }
            Some(SessionCommand::Close) => self.close_requested = true,
            Some(SessionCommand::StartTurn { input } | SessionCommand::Inject { input }) => {
                self.queued_turns.push_back(Queued::Turn(input));
            }
            Some(SessionCommand::Compact) => self.queued_turns.push_back(Queued::Compact),
            Some(SessionCommand::ToolResult { call, .. }) => {
                tracing::debug!(
                    call = call.as_str(),
                    "dropping a tool result for no pending call"
                );
            }
            Some(SessionCommand::Interrupt) => {
                tracing::debug!("interrupt between turns: nothing to stop");
            }
            Some(SessionCommand::SetModel { model }) => {
                self.pending_model = Some(model);
                if self.apply_pending_model() {
                    self.emit_model_changed_reading().await;
                }
            }
            Some(other) => tracing::debug!(?other, "ignoring a command this engine doesn't know"),
        }
    }

    /// A command during a turn. Returns why the turn must stop, if it must.
    pub(crate) fn turn_command(&mut self, command: Option<SessionCommand>) -> Option<Cancel> {
        match command {
            Some(SessionCommand::Close) => {
                self.close_requested = true;
                Some(Cancel::Close)
            }
            None | Some(SessionCommand::Forget) => {
                self.forget_requested = true;
                self.close_requested = true;
                Some(Cancel::Close)
            }
            Some(SessionCommand::Interrupt) => Some(Cancel::Interrupt),
            Some(SessionCommand::StartTurn { input }) => {
                tracing::warn!("StartTurn during a turn; queued for after it");
                self.queued_turns.push_back(Queued::Turn(input));
                None
            }
            Some(SessionCommand::Compact) => {
                tracing::warn!("Compact during a turn; queued for after it");
                self.queued_turns.push_back(Queued::Compact);
                None
            }
            Some(SessionCommand::Inject { input }) => {
                self.steering.push(input);
                None
            }
            Some(SessionCommand::ToolResult { call, result }) => {
                if let Some(slot @ None) = self.pending_results.get_mut(call.as_str()) {
                    *slot = Some(result);
                } else {
                    tracing::debug!(
                        call = call.as_str(),
                        "dropping a tool result for no pending call"
                    );
                }
                None
            }
            Some(SessionCommand::SetModel { model }) => {
                self.pending_model = Some(model);
                None
            }
            Some(other) => {
                tracing::debug!(?other, "ignoring a command this engine doesn't know");
                None
            }
        }
    }

    /// After every turn: the model switch asked for during it, then compaction if the context
    /// is over the threshold, then steering left over becomes the next turn.
    async fn after_turn(&mut self) {
        if self.apply_pending_model() {
            self.emit_model_changed_reading().await;
        }
        if !self.close_requested {
            match self.compact_if_due().await {
                Ok(()) | Err(CompactError::Nothing | CompactError::Cancelled(_)) => {}
                Err(CompactError::Failed(reason)) => {
                    tracing::warn!(%reason, "compaction between turns failed; continuing");
                }
            }
        }
        for input in self.steering.drain(..) {
            self.queued_turns.push_back(Queued::Turn(input));
        }
    }

    /// The old size doesn't apply to a new model's window, so the reading starts unknown.
    async fn emit_model_changed_reading(&mut self) {
        let reading = self.context_reading(None, ContextBasis::SessionStart);
        self.emit(EngineEvent::ContextObserved { reading }).await;
    }

    /// Applies a pending model change; whether the model changed.
    fn apply_pending_model(&mut self) -> bool {
        let Some(model) = self.pending_model.take() else {
            return false;
        };
        if model == self.spec.model {
            return false;
        }
        self.model = self
            .shared
            .models
            .profile(self.spec.account.provider, &model);
        self.spec.model = model.clone();
        if let Some(tools) = &self.builtins {
            tools.model_changed(&model);
        }
        let id = self.history.next_id();
        let at = self.shared.clock.now();
        if let Err(e) = self.history.write(&Entry::ModelChange { id, at, model }) {
            tracing::warn!(error = %e, "couldn't record a model change");
        }
        true
    }

    /// Closes the session: jobs stopped and reported, then `SessionClosed`.
    async fn close(&mut self) {
        if self.forget_requested {
            SessionStore::forget(&self.id);
        }
        if let Some(tools) = self.builtins.clone() {
            tools.shutdown().await;
            let deadline = self.shared.clock.instant() + JOB_SHUTDOWN_WAIT;
            while !self.jobs.running.is_empty() && self.job_notices.is_some() {
                let sleep = self.shared.clock.sleep_until(deadline);
                tokio::select! {
                    notice = recv_notice(&mut self.job_notices) => self.on_notice(notice).await,
                    () = sleep => break,
                }
            }
        }
        let leftover: Vec<JobLabel> = self.jobs.running.iter().cloned().collect();
        for job in leftover {
            self.finish_job(job, None, String::new()).await;
        }
        self.emit(EngineEvent::SessionClosed {
            reason: CloseReason::Requested,
        })
        .await;
    }

    /// Sends an event, keeping the activity gate and the crash supervisor's view current.
    pub(crate) async fn emit(&mut self, event: EngineEvent) {
        let now = self.shared.clock.instant();
        if !matches!(event, EngineEvent::Activity { .. }) {
            self.gate.sent(now);
        }
        {
            let mut s = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &event {
                EngineEvent::TurnStarted => s.turn_open = true,
                EngineEvent::TurnEnded { .. } => s.turn_open = false,
                EngineEvent::JobStarted { job, .. } => {
                    s.running_jobs.insert(job.clone());
                }
                EngineEvent::JobFinished { job, .. } => {
                    s.running_jobs.remove(job);
                }
                EngineEvent::SessionClosed { .. } => s.closed = true,
                _ => {}
            }
        }
        match &event {
            EngineEvent::TurnStarted => self.in_turn = true,
            EngineEvent::TurnEnded { .. } => self.in_turn = false,
            _ => {}
        }
        // The core going away isn't this session's problem: it will close.
        let _ = self.events.send(event).await;
    }

    /// Sends `Activity` if the gate says it is due (inside a turn only).
    pub(crate) async fn activity(&mut self, kind: qm_engine::ActivityKind) {
        if !self.in_turn {
            return;
        }
        let now = self.shared.clock.instant();
        if self.gate.due(now) {
            self.emit(EngineEvent::Activity { kind }).await;
        }
    }

    /// Reports a job's start (from a call's outcome or the notice channel).
    pub(crate) async fn start_job(
        &mut self,
        job: JobLabel,
        command: String,
        log_path: std::path::PathBuf,
    ) {
        if self.jobs.started.contains(&job) || self.jobs.suppressed.contains(&job) {
            return;
        }
        if !self.in_turn {
            tracing::debug!(
                job = job.as_str(),
                "a job started outside a turn; not reported"
            );
            self.jobs.suppressed.insert(job);
            return;
        }
        self.jobs.started.insert(job.clone());
        self.jobs.running.insert(job.clone());
        self.emit(EngineEvent::JobStarted {
            job: job.clone(),
            command,
            log_path: Some(log_path),
            ran_for_ms: None,
        })
        .await;
        if let Some((exit_code, tail)) = self.jobs.early_finished.remove(&job) {
            self.finish_job(job, exit_code, tail).await;
        }
    }

    async fn finish_job(&mut self, job: JobLabel, exit_code: Option<i32>, tail: String) {
        if self.jobs.suppressed.contains(&job) {
            return;
        }
        if !self.jobs.running.remove(&job) {
            if !self.jobs.started.contains(&job) {
                self.jobs.early_finished.insert(job, (exit_code, tail));
            }
            return;
        }
        self.emit(EngineEvent::JobFinished {
            job,
            exit_code,
            tail,
        })
        .await;
    }

    /// A notice from the built-in tools (`None`: the channel closed).
    pub(crate) async fn on_notice(&mut self, notice: Option<JobNotice>) {
        match notice {
            None => self.job_notices = None,
            Some(JobNotice::Started {
                job,
                command,
                log_path,
            }) => self.start_job(job, command, log_path).await,
            Some(JobNotice::Finished {
                job,
                exit_code,
                tail,
            }) => self.finish_job(job, exit_code, tail).await,
        }
    }

    /// Waits for `fut` while serving commands and job notices; with `activity`, sends `Activity`
    /// through the gate every [`ACTIVITY_EVERY`] of silence. Returns early with the reason when a
    /// command cancels the turn.
    pub(crate) async fn wait<F: Future>(
        &mut self,
        fut: F,
        activity: Option<qm_engine::ActivityKind>,
    ) -> Result<F::Output, Cancel> {
        tokio::pin!(fut);
        loop {
            let tick = activity.map(|_| self.shared.clock.sleep(ACTIVITY_EVERY));
            tokio::select! {
                out = &mut fut => return Ok(out),
                command = self.commands.recv() => {
                    if let Some(cancel) = self.turn_command(command) {
                        return Err(cancel);
                    }
                }
                notice = recv_notice(&mut self.job_notices) => self.on_notice(notice).await,
                () = maybe_sleep(tick) => {
                    if let Some(kind) = activity {
                        self.activity(kind).await;
                    }
                }
            }
        }
    }

    /// Like [`Session::wait`], but also wakes when a command changed the session's state (a tool
    /// result arrived), returning `Ok(None)` so the caller can re-check.
    pub(crate) async fn wait_or_command<F: Future>(
        &mut self,
        fut: F,
        activity: Option<qm_engine::ActivityKind>,
    ) -> Result<Option<F::Output>, Cancel> {
        tokio::pin!(fut);
        let tick = activity.map(|_| self.shared.clock.sleep(ACTIVITY_EVERY));
        tokio::select! {
            out = &mut fut => Ok(Some(out)),
            command = self.commands.recv() => match self.turn_command(command) {
                Some(cancel) => Err(cancel),
                None => Ok(None),
            },
            notice = recv_notice(&mut self.job_notices) => {
                self.on_notice(notice).await;
                Ok(None)
            }
            () = maybe_sleep(tick) => {
                if let Some(kind) = activity {
                    self.activity(kind).await;
                }
                Ok(None)
            }
        }
    }

    /// Appends a message to the context and the history.
    pub(crate) fn record(&mut self, message: Message) {
        let at = self.shared.clock.now();
        let id = match self.history.append_message(at, &message) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't record a message in the session history");
                self.history.next_id()
            }
        };
        self.context.push(id, message);
    }

    /// The adapter for the current account and credential kind.
    pub(crate) fn adapter_for(
        &self,
        kind: &adapters::CredentialKind,
    ) -> Option<Box<dyn adapters::WireAdapter>> {
        adapters::adapter_for(self.spec.account.provider, kind)
    }
}

async fn recv_notice(rx: &mut Option<mpsc::UnboundedReceiver<JobNotice>>) -> Option<JobNotice> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn maybe_sleep(sleep: Option<qm_types::clock::Sleep>) {
    match sleep {
        Some(s) => s.await,
        None => std::future::pending().await,
    }
}
