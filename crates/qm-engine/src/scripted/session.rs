//! Playing one session's script.
//!
//! One task per session. Between turns it waits for a command, a scripted job's exit, or the idle
//! shutdown; in a turn it plays the script's calls step by step. Every wait goes through the
//! session's [`Clock`] and listens for commands, so `Interrupt` and `Close` stop at the current
//! step and scripted jobs finish on time, between turns too.

use super::Progress;
use super::script::{CallScript, Script, Step};
use crate::activity::{ACTIVITY_EVERY, ActivityGate};
use crate::event::{
    ActivityKind, CloseReason, EngineEvent, FailureKind, InitReport, RateLimitInfo,
    RateLimitStatus, StopReason, TurnFailure, TurnOutcome, TurnUsage,
};
use crate::lines::LineBuffer;
use crate::session::{EngineSide, InputBlock, SessionCommand, TurnInput};
use crate::spec::{Builtins, SessionSpec};
use crate::tool::{ToolCallId, ToolResult};
use qm_types::clock::Clock;
use qm_types::domain::Role;
use qm_types::ids::{JobLabel, SessionRef};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The file tools a `ProviderFileTools` session offers (as `qm-providers` names them).
const PROVIDER_FILE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "grep",
    "find",
    "ls",
    "bash",
    "job_output",
    "job_kill",
];
/// The helper-agent tool a crewmate's `ProviderFileTools` session also offers.
const PROVIDER_HELPER_TOOL: &str = "agent";
/// Claude Code's main built-in tools, for a `ClaudeCode` session's init report.
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Bash",
    "Edit",
    "Glob",
    "Grep",
    "Read",
    "Write",
    "WebFetch",
    "WebSearch",
];
/// Retries per call when the spec carries no limits ("the provider's 2 retries").
const DEFAULT_MAX_RETRIES: u32 = 2;

pub(super) struct Session {
    side: EngineSide,
    spec: SessionSpec,
    script: Arc<Script>,
    index: Option<usize>,
    clock: Arc<dyn Clock>,
    progress: Arc<Mutex<Progress>>,
    gate: ActivityGate,
    /// Scripted jobs still running, in start order.
    jobs: Vec<PendingJob>,
    /// The process is up (not suspended or crashed).
    up: bool,
    /// `EndConversation` was played in this turn.
    ending: bool,
    in_turn: bool,
    /// When the session last became idle (no turn, no job), for the idle shutdown.
    idle_since: Option<Instant>,
    /// Commands that arrived mid-turn and wait for the turn to end.
    queued: VecDeque<SessionCommand>,
    results: HashMap<ToolCallId, ToolResult>,
    injects: VecDeque<TurnInput>,
}

struct PendingJob {
    label: JobLabel,
    at: Instant,
    exit_code: Option<i32>,
    tail: String,
}

/// Why a turn stopped before its script ran out.
enum Stop {
    Interrupted,
    Close,
}

/// What a wait was for, and what it got.
enum Want {
    Nothing,
    Result(ToolCallId),
    Inject,
}

enum Got {
    Elapsed,
    Result(ToolResult),
    Inject,
}

/// How a played turn ended.
enum Played {
    Completed,
    Failed(FailureKind, String),
    Crashed,
}

impl Session {
    pub(super) fn new(
        side: EngineSide,
        spec: SessionSpec,
        script: Arc<Script>,
        index: Option<usize>,
        clock: Arc<dyn Clock>,
        progress: Arc<Mutex<Progress>>,
    ) -> Self {
        Self {
            side,
            spec,
            script,
            index,
            clock,
            progress,
            gate: ActivityGate::new(),
            jobs: Vec::new(),
            up: false,
            ending: false,
            in_turn: false,
            idle_since: None,
            queued: VecDeque::new(),
            results: HashMap::new(),
            injects: VecDeque::new(),
        }
    }

    pub(super) async fn run(mut self) {
        let resumed = self.spec.resume.is_some();
        let Some(index) = self.index else {
            let message = format!(
                "the script {} has no session {}",
                self.script.name,
                self.spec
                    .resume
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string)
            );
            self.emit(EngineEvent::SessionClosed {
                reason: CloseReason::ResumeFailed { message },
            })
            .await;
            return;
        };
        if resumed && !self.script.sessions[index].resumable {
            self.emit(EngineEvent::SessionClosed {
                reason: CloseReason::ResumeFailed {
                    message: "this scripted session can't be resumed".into(),
                },
            })
            .await;
            return;
        }
        self.ready(index, resumed).await;

        loop {
            let command = match self.queued.pop_front() {
                Some(c) => Some(c),
                None => self.wait_idle().await,
            };
            match command {
                Some(SessionCommand::StartTurn { input } | SessionCommand::Inject { input }) => {
                    if let Some(reason) = self.turn(index, &input).await {
                        self.close(reason).await;
                        return;
                    }
                }
                Some(SessionCommand::Close) | None => {
                    self.close(CloseReason::Requested).await;
                    return;
                }
                // Between turns these change nothing: no turn to interrupt, no call waiting, and
                // the scripted model plays the same whatever the model.
                Some(
                    SessionCommand::Interrupt
                    | SessionCommand::ToolResult { .. }
                    | SessionCommand::SetModel { .. },
                ) => {}
            }
        }
    }

    async fn ready(&mut self, index: usize, resumed: bool) {
        let agent = self.spec.agent.to_string();
        let session = SessionRef::new(format!("scripted-{agent}-{}", index + 1))
            .expect("a scripted session reference is never empty");
        let mut tools: Vec<String> = self.spec.tools.iter().map(|t| t.name.clone()).collect();
        match &self.spec.builtins {
            Builtins::None => {}
            Builtins::ClaudeCode => tools.extend(CLAUDE_CODE_TOOLS.iter().map(|t| (*t).to_owned())),
            Builtins::ProviderFileTools { .. } => {
                tools.extend(PROVIDER_FILE_TOOLS.iter().map(|t| (*t).to_owned()));
                // As the provider engine: helper agents for crewmates only.
                if self.spec.role == Role::Crewmate {
                    tools.push(PROVIDER_HELPER_TOOL.to_owned());
                }
            }
        }
        let init = InitReport {
            engine_version: Some("scripted/1".into()),
            model: Some(self.spec.model.clone()),
            tools,
            permission_mode: None,
            capabilities: Vec::new(),
            raw: serde_json::json!({ "script": self.script.name }),
        };
        self.emit(EngineEvent::SessionReady {
            session,
            resumed,
            init,
        })
        .await;
        self.up = true;
        self.idle_since = Some(self.clock.instant());
    }

    /// Waits between turns for the next command, finishing jobs and suspending on the way.
    /// `None` once the core has dropped its handle.
    async fn wait_idle(&mut self) -> Option<SessionCommand> {
        loop {
            let job_at = self.next_job_at();
            let idle_at = match (self.up, self.spec.idle_shutdown, self.idle_since) {
                (true, Some(after), Some(since)) if self.jobs.is_empty() => Some(since + after),
                _ => None,
            };
            let clock = Arc::clone(&self.clock);
            tokio::select! {
                command = self.side.commands.recv() => return command,
                () = sleep_opt(&*clock, job_at) => self.finish_due_jobs().await,
                () = sleep_opt(&*clock, idle_at) => {
                    self.emit(EngineEvent::Suspended).await;
                    self.up = false;
                    self.idle_since = None;
                }
            }
        }
    }

    /// Plays one turn; `Some(reason)` when the session closes after it.
    async fn turn(&mut self, index: usize, input: &TurnInput) -> Option<CloseReason> {
        if !self.up {
            if !self.script.sessions[index].resumable {
                return Some(CloseReason::ResumeFailed {
                    message: "this scripted session can't be resumed".into(),
                });
            }
            self.ready(index, true).await;
        }
        let number = {
            let mut progress = super::lock(&self.progress);
            progress.next_turn[index] += 1;
            progress.next_turn[index]
        };
        self.idle_since = None;
        self.in_turn = true;
        self.emit(EngineEvent::TurnStarted).await;
        let started = self.clock.instant();
        let mut usage = TurnUsage::default();
        let mut final_text = String::new();

        let played = self
            .play(index, number, input, &mut usage, &mut final_text)
            .await;
        let mut close = None;
        let outcome = match played {
            Ok(Played::Completed) => TurnOutcome::Completed,
            Ok(Played::Failed(kind, message)) => TurnOutcome::Failed {
                failure: TurnFailure { kind, message },
            },
            Ok(Played::Crashed) => {
                self.finish_all_jobs().await;
                self.up = false;
                TurnOutcome::Failed {
                    failure: TurnFailure {
                        kind: FailureKind::ProcessExited,
                        message: "the scripted process exited (a crash step)".into(),
                    },
                }
            }
            Err(Stop::Interrupted) => TurnOutcome::Interrupted,
            Err(Stop::Close) => {
                close = Some(CloseReason::Requested);
                TurnOutcome::Interrupted
            }
        };
        self.in_turn = false;
        usage.duration_ms = duration_ms(self.clock.instant().saturating_duration_since(started));
        self.emit(EngineEvent::TurnEnded {
            outcome,
            usage,
            final_text,
        })
        .await;
        // Whatever was injected into the turn was delivered with it; stray results are dropped.
        self.injects.clear();
        self.results.clear();
        if self.ending {
            return Some(CloseReason::EndConversation);
        }
        self.idle_since = Some(self.clock.instant());
        close
    }

    async fn play(
        &mut self,
        index: usize,
        number: usize,
        input: &TurnInput,
        usage: &mut TurnUsage,
        final_text: &mut String,
    ) -> Result<Played, Stop> {
        let script = Arc::clone(&self.script);
        let Some(turn) = script.sessions[index].turns.get(number - 1) else {
            return Ok(Played::Failed(
                FailureKind::Internal,
                format!("the script has no turn {number} for {}", self.spec.agent),
            ));
        };
        if let Some(expected) = &turn.expect_input_contains
            && !input_text(input).contains(expected.as_str())
        {
            return Ok(Played::Failed(
                FailureKind::Internal,
                format!("turn {number} expected input containing {expected:?}"),
            ));
        }
        let mut tool_calls = 0u32;
        for (c, call) in turn.calls.iter().enumerate() {
            let call_no = u32::try_from(c + 1).unwrap_or(u32::MAX);
            self.emit(EngineEvent::ModelCallStarted { call: call_no })
                .await;
            if let Some(failed) = self.provider_trouble(call).await? {
                return Ok(failed);
            }
            let mut text = Vec::new();
            let mut stop = StopReason::EndTurn;
            for step in &call.steps {
                match self.step(number, step, &mut tool_calls, &mut text).await? {
                    StepDone::Next => {}
                    StepDone::UsedTool => stop = StopReason::ToolUse,
                    StepDone::Fail(message) => {
                        return Ok(Played::Failed(FailureKind::Internal, message));
                    }
                    StepDone::Crash => return Ok(Played::Crashed),
                }
            }
            self.emit(EngineEvent::ModelCallFinished {
                call: call_no,
                usage: call.usage,
                stop,
            })
            .await;
            usage.model_calls += 1;
            usage.input_tokens += call.usage.input_tokens;
            usage.output_tokens += call.usage.output_tokens;
            usage.thinking_tokens += call.usage.thinking_tokens;
            usage.cache_read_tokens += call.usage.cache_read_tokens;
            usage.cache_write_tokens += call.usage.cache_write_tokens;
            *final_text = text.join("\n");
        }
        Ok(Played::Completed)
    }

    /// A call's scripted provider errors (each retried, until `max_retries` runs out) and
    /// account limit.
    async fn provider_trouble(&mut self, call: &CallScript) -> Result<Option<Played>, Stop> {
        let max = self
            .spec
            .limits
            .map_or(DEFAULT_MAX_RETRIES, |l| l.max_retries);
        for (i, error) in call.errors.iter().enumerate() {
            let attempt = u32::try_from(i).unwrap_or(u32::MAX);
            if attempt >= max {
                return Ok(Some(Played::Failed(
                    FailureKind::Provider,
                    format!(
                        "the provider kept failing after {max} retries: {}",
                        error.message
                    ),
                )));
            }
            let delay_ms = (500u64 << attempt.min(5)).min(8_000);
            self.emit(EngineEvent::Retrying {
                attempt: attempt + 1,
                max_attempts: max,
                error: error.clone(),
                delay_ms,
            })
            .await;
            self.pause(Duration::from_millis(delay_ms)).await?;
        }
        if let Some(limit) = &call.account_limit {
            let resets_at = self
                .clock
                .now()
                .checked_add(Duration::from_millis(limit.resets_in_ms))
                .ok();
            self.emit(EngineEvent::RateLimit {
                info: RateLimitInfo {
                    status: RateLimitStatus::Rejected,
                    window: Some(limit.window.clone()),
                    resets_at,
                },
            })
            .await;
            return Ok(Some(Played::Failed(
                FailureKind::AccountLimit,
                format!("the account hit its {} usage limit", limit.window),
            )));
        }
        Ok(None)
    }

    #[allow(clippy::too_many_lines)] // One arm per step kind.
    async fn step(
        &mut self,
        turn: usize,
        step: &Step,
        tool_calls: &mut u32,
        text: &mut Vec<String>,
    ) -> Result<StepDone, Stop> {
        match step {
            Step::Text(t) => {
                let mut buffer = LineBuffer::default();
                let mut lines = buffer.push(t);
                lines.extend(buffer.flush());
                for line in lines {
                    text.push(line.clone());
                    self.emit(EngineEvent::TextLine { text: line }).await;
                }
            }
            Step::Thinking { ms } => self.work(*ms, ActivityKind::Thinking).await?,
            Step::ToolCall {
                name,
                args,
                expect_result_contains,
            } => {
                *tool_calls += 1;
                let id = ToolCallId::new(format!("call-{turn}-{tool_calls}"));
                self.emit(EngineEvent::ToolCall {
                    call: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                })
                .await;
                let Got::Result(result) = self.wait(None, Want::Result(id.clone())).await? else {
                    unreachable!("a wait with no deadline ends only with what it waited for");
                };
                if let Some(expected) = expect_result_contains
                    && !result.text.contains(expected.as_str())
                {
                    return Ok(StepDone::Fail(format!(
                        "tool call {} ({name}) expected a result containing {expected:?}, got \
                         {:?}",
                        id.as_str(),
                        result.text
                    )));
                }
                return Ok(StepDone::UsedTool);
            }
            Step::Builtin {
                name,
                summary,
                for_ms,
            } => {
                self.emit(EngineEvent::BuiltinToolUse {
                    name: name.clone(),
                    summary: summary.clone(),
                })
                .await;
                self.work(*for_ms, ActivityKind::BuiltinTool).await?;
            }
            Step::Job {
                label,
                command,
                finish_after_ms,
                exit_code,
                tail,
            } => {
                let label = JobLabel::new(label.clone()).expect("validated when the script loaded");
                self.emit(EngineEvent::JobStarted {
                    job: label.clone(),
                    command: command.clone(),
                    log_path: None,
                    ran_for_ms: None,
                })
                .await;
                self.jobs.push(PendingJob {
                    label,
                    at: self.clock.instant() + Duration::from_millis(*finish_after_ms),
                    exit_code: *exit_code,
                    tail: tail.clone(),
                });
            }
            Step::Stall { ms } => self.pause(Duration::from_millis(*ms)).await?,
            Step::Crash => return Ok(StepDone::Crash),
            Step::RateLimit(status) => {
                self.emit(EngineEvent::RateLimit {
                    info: RateLimitInfo {
                        status: *status,
                        window: None,
                        resets_at: None,
                    },
                })
                .await;
            }
            Step::Compact { before, after } => {
                self.emit(EngineEvent::Compacted {
                    automatic: true,
                    tokens_before: Some(*before),
                    tokens_after: Some(*after),
                })
                .await;
            }
            Step::EndConversation => {
                self.emit(EngineEvent::EndConversationCalled).await;
                self.ending = true;
            }
            Step::WaitForInject { ms } => {
                let deadline = self.clock.instant() + Duration::from_millis(*ms);
                if let Got::Elapsed = self.wait(Some(deadline), Want::Inject).await? {
                    return Ok(StepDone::Fail(format!(
                        "waited {ms} ms for an injected message; none came"
                    )));
                }
            }
        }
        Ok(StepDone::Next)
    }

    /// Silent work for `ms`: `Activity` whenever the gate says it's due (every 30 s of clock).
    async fn work(&mut self, ms: u64, kind: ActivityKind) -> Result<(), Stop> {
        if ms == 0 {
            return Ok(());
        }
        let end = self.clock.instant() + Duration::from_millis(ms);
        loop {
            let now = self.clock.instant();
            if self.gate.due(now) {
                self.send(EngineEvent::Activity { kind }).await;
            }
            if now >= end {
                return Ok(());
            }
            self.wait(Some(end.min(now + ACTIVITY_EVERY)), Want::Nothing)
                .await?;
        }
    }

    /// Nothing at all for `duration`, unless a command stops the turn.
    async fn pause(&mut self, duration: Duration) -> Result<(), Stop> {
        let deadline = self.clock.instant() + duration;
        self.wait(Some(deadline), Want::Nothing).await.map(drop)
    }

    /// Waits in a turn until `deadline` or until `want` arrives, handling commands and finishing
    /// scripted jobs on the way.
    async fn wait(&mut self, deadline: Option<Instant>, want: Want) -> Result<Got, Stop> {
        loop {
            match &want {
                Want::Result(id) => {
                    if let Some(result) = self.results.remove(id) {
                        return Ok(Got::Result(result));
                    }
                }
                Want::Inject => {
                    if self.injects.pop_front().is_some() {
                        return Ok(Got::Inject);
                    }
                }
                Want::Nothing => {}
            }
            if deadline.is_some_and(|d| self.clock.instant() >= d) {
                return Ok(Got::Elapsed);
            }
            let job_at = self.next_job_at();
            let clock = Arc::clone(&self.clock);
            tokio::select! {
                command = self.side.commands.recv() => match command {
                    None | Some(SessionCommand::Close) => return Err(Stop::Close),
                    Some(SessionCommand::Interrupt) => return Err(Stop::Interrupted),
                    Some(SessionCommand::ToolResult { call, result }) => {
                        self.results.insert(call, result);
                    }
                    Some(SessionCommand::Inject { input }) => self.injects.push_back(input),
                    Some(SessionCommand::StartTurn { input }) => {
                        self.queued.push_back(SessionCommand::StartTurn { input });
                    }
                    Some(SessionCommand::SetModel { .. }) => {}
                },
                () = sleep_opt(&*clock, job_at) => self.finish_due_jobs().await,
                () = sleep_opt(&*clock, deadline) => {}
            }
        }
    }

    fn next_job_at(&self) -> Option<Instant> {
        self.jobs.iter().map(|j| j.at).min()
    }

    /// Reports every job whose time has come, earliest first.
    async fn finish_due_jobs(&mut self) {
        let now = self.clock.instant();
        let mut due: Vec<PendingJob> = Vec::new();
        let mut i = 0;
        while i < self.jobs.len() {
            if self.jobs[i].at <= now {
                due.push(self.jobs.remove(i));
            } else {
                i += 1;
            }
        }
        due.sort_by_key(|j| j.at);
        for job in due {
            self.emit(EngineEvent::JobFinished {
                job: job.label,
                exit_code: job.exit_code,
                tail: job.tail,
            })
            .await;
        }
        // The last job ending between turns restarts the idle time.
        if self.jobs.is_empty() && !self.in_turn && self.up {
            self.idle_since = Some(now);
        }
    }

    /// Every job still running dies with the process or the session: reported with no exit code.
    async fn finish_all_jobs(&mut self) {
        for job in std::mem::take(&mut self.jobs) {
            self.emit(EngineEvent::JobFinished {
                job: job.label,
                exit_code: None,
                tail: String::new(),
            })
            .await;
        }
    }

    async fn close(&mut self, reason: CloseReason) {
        self.finish_all_jobs().await;
        self.emit(EngineEvent::SessionClosed { reason }).await;
    }

    /// Sends a committed event: any event shows the session is alive.
    async fn emit(&mut self, event: EngineEvent) {
        self.gate.sent(self.clock.instant());
        self.send(event).await;
    }

    async fn send(&mut self, event: EngineEvent) {
        // A core that dropped its handle no longer listens; the session ends when its command
        // channel closes.
        let _ = self.side.events.send(event).await;
    }
}

/// What a step left behind.
enum StepDone {
    Next,
    UsedTool,
    Fail(String),
    Crash,
}

async fn sleep_opt(clock: &dyn Clock, deadline: Option<Instant>) {
    match deadline {
        Some(d) => clock.sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn input_text(input: &TurnInput) -> String {
    input
        .blocks
        .iter()
        .filter_map(|b| match b {
            InputBlock::Text { text } => Some(text.as_str()),
            InputBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
