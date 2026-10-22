//! One Claude session: the task that owns its `claude` process.
//!
//! ```text
//! Starting ──initialize ok──▶ Idle ◀──result──┐
//!    │                         │ StartTurn     │
//!    │                         ▼               │
//!    │                       InTurn ───────────┘
//!    │                         │ crash (exit)            idle timer, no job
//!    ▼                         ▼                          ▼
//!  Closed ◀── Close ──────── Down ◀── Suspended ◀────── Idle
//!                              │ StartTurn/Inject: spawn --resume,
//!                              └─▶ initialize ok: SessionReady{resumed:true} ─▶ InTurn
//! ```
//!
//! The task owns the child (through the supervisor), a writer task for its stdin, a reader task
//! for its stdout (lines up to 16 MiB; longer ones are dropped with a warning), a bounded stderr
//! tail, the exit watch, the command receiver and the event sender. Everything it waits on is
//! one `select!` (`Session::wait`); every timer runs on the injected clock.
//!
//! - **Open.** Spawn, `initialize`, wait up to 60 s for its answer while serving the MCP
//!   handshake, then ask `get_context_usage` and `get_hooks_listing` for the init report, then
//!   `SessionReady`. Commands sent meanwhile wait in the channel.
//! - **Turn.** The user message, `TurnStarted`, the stream mapped to events, `result` ends it.
//! - **Interrupt.** `interrupt`; the next `result` ends the turn `Interrupted`. With no `result`
//!   within 10 s the process is stopped, jobs are reported finished, the turn ends `Interrupted`
//!   and the session is `Suspended`.
//! - **Idle shutdown.** After `spec.idle_shutdown` with no turn and no job: stop,
//!   `Suspended`. The next turn resumes with `--resume=<id>`.
//! - **Crash.** An exit nobody asked for: drain stdout for at most 200 ms, report running jobs
//!   finished, then fail the turn (`ProcessExited`) or, between turns, report `Suspended`.
//! - **`EndConversation`.** Reported, the turn ends normally, then stop and
//!   `SessionClosed { EndConversation }`.

pub mod stream;

use crate::argv::{LaunchOptions, ToolSet, build_argv};
use crate::autocompact::{self, Restart};
use crate::check::{ExpectedSession, REQUIRED_CAPABILITIES};
use crate::env::build_env;
use crate::jobs::Jobs;
use crate::mcp::{McpOutcome, McpServer};
use crate::protocol::{self, Incoming, redact_accounts, str_at};
use crate::{SERVER_NAME, Shared, plugin, transcript};
use qm_engine::supervisor::{
    Child, ChildOutput, ChildStdin, ExitCause, ExitWatch, ProcessOwner, SpawnSpec, StopPolicy, stop,
};
use qm_engine::{
    ACTIVITY_EVERY, ActivityGate, ActivityKind, CallUsage, CloseReason, EngineEvent, EngineSide,
    FailureKind, InitReport, LineBuffer, SessionCommand, SessionSpec, StopReason, SystemPrompt,
    ToolCallId, TurnFailure, TurnInput, TurnOutcome,
};
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::ids::{ModelId, SessionRef};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

/// The longest stdout line kept; longer ones are dropped with a warning.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// How much of stderr is kept for crash messages.
const STDERR_TAIL: usize = 8 * 1024;
/// How long `initialize` may take to be answered.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long each zero-token init query may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a turn may take to end after our `interrupt` before the process is stopped.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long stdout is drained after an unrequested exit.
const CRASH_DRAIN: Duration = Duration::from_millis(200);
/// The message that makes `claude` compact its conversation (`SessionCommand::Compact`).
const COMPACT_COMMAND: &str = "/compact";

/// One session's task. Built by `ClaudeEngine::open` and run on the Tokio runtime.
// The flags are independent facts that overlap (a session can be closing while its process
// starts, or ending while it crashes); an enum would have to spell out every combination.
#[allow(clippy::struct_excessive_bools)]
pub struct Session {
    shared: Arc<Shared>,
    clock: Arc<dyn Clock>,
    spec: SessionSpec,
    /// The model the next process starts with (`SetModel` changes it).
    model: String,
    /// Our session ID: the `--session-id` we chose, or the one resumed.
    conversation: String,
    /// The app's settings as they change; the autocompact window is read here.
    settings: watch::Receiver<Config>,
    /// False once the settings channel closed (nothing can change them any more).
    settings_open: bool,
    /// The autocompact window the current process was launched with.
    launched_window: u32,
    tool_set: ToolSet,
    mcp: McpServer,
    expected: ExpectedSession,
    events: mpsc::Sender<EngineEvent>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    proc: Option<Proc>,
    /// The process has been started once and is now down (suspended or crashed).
    down: bool,
    turn: Option<Turn>,
    jobs: Jobs,
    gate: ActivityGate,
    last_event_at: Instant,
    next_request: u64,
    /// Our tool calls waiting for the core's result, by call ID.
    pending_calls: HashMap<String, PendingCall>,
    /// A `SetModel` to send at the next idle moment.
    pending_model: Option<String>,
    idle_deadline: Option<Instant>,
    start_deadline: Option<Instant>,
    /// Checks of the first `system/init`, when the init report couldn't make them.
    init_checks: Option<InitChecks>,
    /// What the current process has spent so far, as its last turn's `result` reported it
    /// (`stream::Spent`): a turn's usage is the difference, so helper agents and turns `claude`
    /// ran by itself count in the next turn. Set when a process starts: nothing for a new
    /// session, the transcript's saved totals for a resumed one ([`saved_spent`]).
    spent: stream::Spent,
    /// `claude` started a turn by itself (after a background task finished) and was interrupted;
    /// its `result` is not ours.
    unasked: bool,
    /// This process reports `command_lifecycle` (claude 2.1.304 does, from its first message);
    /// without it, a turn ends at its first `result`, as before.
    lifecycle_seen: bool,
    /// Set when `claude` failed a check of its `system/init` (tools outside the role's grant, a
    /// foreign session, the wrong permission mode, a missing capability): the process was
    /// stopped, and every turn fails at once with this sentence, never starting `claude` again.
    refused: Option<String>,
    /// No events go out while the process starts: the stream's order allows none.
    starting: bool,
    closing: bool,
    /// `EndConversationCalled` was sent: the session closes once the turn ends.
    ending: bool,
    /// The core dropped its handle: nobody reads events any more.
    core_gone: bool,
    done: bool,
    claude_home: Option<PathBuf>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("agent", &self.spec.agent)
            .field("session_id", &self.conversation)
            .field("running", &self.proc.is_some())
            .field("down", &self.down)
            .field("in_turn", &self.turn.is_some())
            .finish_non_exhaustive()
    }
}

/// A running `claude`.
struct Proc {
    child: Child,
    writer: Option<mpsc::UnboundedSender<Vec<u8>>>,
    lines: mpsc::UnboundedReceiver<Vec<u8>>,
    lines_open: bool,
    exit: ExitWatch,
    exited: Option<ExitCause>,
    stderr: Arc<Mutex<String>>,
    stderr_done: watch::Receiver<bool>,
}

#[derive(Debug)]
struct PendingCall {
    request_id: Value,
    rpc_id: Value,
}

/// One turn's running state.
#[derive(Debug)]
struct Turn {
    started: Instant,
    calls: u32,
    call_open: bool,
    call_usage: CallUsage,
    call_stop: StopReason,
    /// What the turn's finished calls used, summed: its usage when `claude` dies before its
    /// `result`.
    spent: CallUsage,
    lines: LineBuffer,
    /// Content block types by index, in the current message.
    blocks: HashMap<u64, String>,
    /// The current call's text, for `final_text` when `result` has none.
    call_text: String,
    seen_tool_uses: HashSet<String>,
    builtin_running: BTreeSet<String>,
    interrupt_deadline: Option<Instant>,
    interrupted: bool,
    /// A failure the engine decided on (output cap, protocol); it wins over the result.
    failure: Option<TurnFailure>,
    rate_limited: bool,
    /// The messages sent into this turn after it started (`Inject`), by uuid: whether `claude`
    /// has started on each (`command_lifecycle`). One sent while a model call is writing its
    /// answer is queued, and `claude` answers it in another query after the turn's first
    /// `result`; the turn ends only when none is still waiting.
    injects: BTreeMap<String, bool>,
}

impl Turn {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            calls: 0,
            call_open: false,
            call_usage: CallUsage::default(),
            call_stop: StopReason::Unknown,
            spent: CallUsage::default(),
            lines: LineBuffer::default(),
            blocks: HashMap::new(),
            call_text: String::new(),
            seen_tool_uses: HashSet::new(),
            builtin_running: BTreeSet::new(),
            interrupt_deadline: None,
            interrupted: false,
            failure: None,
            rate_limited: false,
            injects: BTreeMap::new(),
        }
    }
}

/// What to verify in the first `system/init`, besides the tools (checked at every one).
#[derive(Debug)]
struct InitChecks {
    /// The required capabilities, when `initialize` didn't advertise them.
    capabilities: bool,
    permission_mode: Option<String>,
}

/// Why starting a process failed.
#[derive(Debug)]
enum StartFailure {
    Spawn(String),
    Exited {
        cause: Option<ExitCause>,
        stderr: String,
    },
    Refused(String),
    /// The session's tools failed the check.
    Untrusted(String),
    Timeout,
}

impl StartFailure {
    fn sentence(&self) -> String {
        match self {
            Self::Spawn(message) => format!("couldn't start claude: {message}"),
            Self::Exited { cause, stderr } => {
                let mut s = format!("claude exited while starting ({})", describe_exit(*cause));
                if let Some(line) = last_line(stderr) {
                    s.push_str(": ");
                    s.push_str(line);
                }
                s
            }
            Self::Refused(error) => format!("claude refused to start the session: {error}"),
            Self::Untrusted(problem) => format!("{problem}, so the session was refused"),
            Self::Timeout => "claude didn't answer initialize within 60 seconds".into(),
        }
    }
}

/// What woke the task.
enum Wake {
    Line(Vec<u8>),
    LinesClosed,
    Exited(ExitCause),
    Command(Option<SessionCommand>),
    /// The settings changed (`false`: the channel closed).
    Settings(bool),
    Timer(Timer),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    Start,
    Interrupt,
    Idle,
    Activity,
}

/// The running totals a process starts from: a resumed `claude` restores them from the last
/// `cost-state` in its transcript (so its first `result` counts every earlier process of the
/// session again), a new one starts from nothing.
async fn saved_spent(claude_home: Option<PathBuf>, session_id: String) -> stream::Spent {
    let Some(home) = claude_home else {
        return stream::Spent::default();
    };
    match tokio::task::spawn_blocking(move || transcript::last_cost_state(&home, &session_id)).await
    {
        Ok(Some(state)) => stream::saved_spent(&state),
        Ok(None) => stream::Spent::default(),
        Err(e) => {
            tracing::warn!("the transcript cost-state task failed: {e}");
            stream::Spent::default()
        }
    }
}

impl Session {
    pub(crate) fn new(shared: Arc<Shared>, spec: SessionSpec, side: EngineSide) -> Self {
        let clock = Arc::clone(&shared.config.clock);
        let now = clock.instant();
        let session_id = spec.resume.as_ref().map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |r| r.as_str().to_owned(),
        );
        let tool_set = ToolSet::for_role(spec.role);
        let mcp = McpServer::new(spec.tools.clone());
        let expected = ExpectedSession::for_spec(&spec);
        let claude_home = transcript::claude_home(&shared.config.inherited_env);
        Self {
            model: spec.model.as_str().to_owned(),
            clock,
            conversation: session_id,
            settings: shared.config.settings.clone(),
            settings_open: true,
            launched_window: 0,
            tool_set,
            mcp,
            expected,
            events: side.events,
            commands: side.commands,
            proc: None,
            down: false,
            turn: None,
            jobs: Jobs::default(),
            gate: ActivityGate::new(),
            last_event_at: now,
            next_request: 0,
            pending_calls: HashMap::new(),
            pending_model: None,
            idle_deadline: None,
            start_deadline: None,
            init_checks: None,
            refused: None,
            spent: stream::Spent::default(),
            unasked: false,
            lifecycle_seen: false,
            starting: false,
            closing: false,
            ending: false,
            core_gone: false,
            done: false,
            claude_home,
            shared,
            spec,
        }
    }

    /// Runs the session to its end.
    pub async fn run(mut self) {
        let resuming = self.spec.resume.is_some();
        match self.start(resuming).await {
            Ok(init) => {
                self.send(EngineEvent::SessionReady {
                    session: self.session_ref(),
                    resumed: resuming,
                    init,
                })
                .await;
            }
            Err(failure) => {
                let reason = if resuming {
                    CloseReason::ResumeFailed {
                        message: format!(
                            "couldn't resume the Claude session {}: {}",
                            self.conversation,
                            failure.sentence()
                        ),
                    }
                } else {
                    crashed(&failure)
                };
                self.send(EngineEvent::SessionClosed { reason }).await;
                return;
            }
        }
        while !self.done {
            let wake = self.wait().await;
            self.on_wake(wake).await;
            if self.core_gone && !self.done {
                self.finish(CloseReason::Requested).await;
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Waiting
    // ---------------------------------------------------------------------------------------

    fn next_timer(&self) -> Option<(Instant, Timer)> {
        let mut timers: Vec<(Instant, Timer)> = Vec::new();
        if let Some(at) = self.start_deadline {
            timers.push((at, Timer::Start));
        }
        if let Some(at) = self.turn.as_ref().and_then(|t| t.interrupt_deadline) {
            timers.push((at, Timer::Interrupt));
        }
        if let Some(at) = self.idle_deadline {
            timers.push((at, Timer::Idle));
        }
        if let Some(turn) = &self.turn
            && !self.starting
            && (turn.call_open || !turn.builtin_running.is_empty())
        {
            timers.push((self.last_event_at + ACTIVITY_EVERY, Timer::Activity));
        }
        timers.into_iter().min_by_key(|(at, _)| *at)
    }

    async fn wait(&mut self) -> Wake {
        let timer = self.next_timer();
        let clock = Arc::clone(&self.clock);
        let sleep = async move {
            match timer {
                Some((at, kind)) => {
                    clock.sleep_until(at).await;
                    kind
                }
                None => std::future::pending().await,
            }
        };
        let accept_commands = !self.starting && !self.closing && !self.done;
        let watch_settings = accept_commands && self.settings_open;
        let (lines, exit) = match &mut self.proc {
            Some(proc) => (
                proc.lines_open.then_some(&mut proc.lines),
                proc.exited.is_none().then_some(&mut proc.exit),
            ),
            None => (None, None),
        };
        let commands = &mut self.commands;
        let settings = &mut self.settings;
        tokio::select! {
            biased;
            line = recv_line(lines) => match line {
                Some(line) => Wake::Line(line),
                None => Wake::LinesClosed,
            },
            cause = wait_exit(exit) => Wake::Exited(cause),
            command = commands.recv(), if accept_commands => Wake::Command(command),
            changed = settings.changed(), if watch_settings => Wake::Settings(changed.is_ok()),
            kind = sleep => Wake::Timer(kind),
        }
    }

    async fn on_wake(&mut self, wake: Wake) {
        match wake {
            Wake::Line(line) => self.on_line(&line).await,
            Wake::LinesClosed => {
                if let Some(proc) = &mut self.proc {
                    proc.lines_open = false;
                }
            }
            Wake::Exited(cause) => self.on_exit(cause).await,
            Wake::Command(None) => {
                self.core_gone = true;
            }
            Wake::Command(Some(command)) => self.on_command(command).await,
            Wake::Settings(open) => {
                self.settings_open = open;
                self.restart_for_settings().await;
            }
            Wake::Timer(timer) => self.on_timer(timer).await,
        }
    }

    // ---------------------------------------------------------------------------------------
    // Starting and resuming
    // ---------------------------------------------------------------------------------------

    /// Spawns `claude`, runs `initialize` and the init queries. No events go out meanwhile.
    async fn start(&mut self, resuming: bool) -> Result<InitReport, StartFailure> {
        self.starting = true;
        // Only the start deadline may fire while starting.
        self.idle_deadline = None;
        let result = self.start_inner(resuming).await;
        self.starting = false;
        self.start_deadline = None;
        if result.is_err() {
            self.stop_process().await;
        }
        result
    }

    async fn start_inner(&mut self, resuming: bool) -> Result<InitReport, StartFailure> {
        let saved = saved_spent(
            self.claude_home.clone().filter(|_| resuming),
            self.conversation.clone(),
        )
        .await;

        let plugin_dir = match (&self.spec.skills_dir, self.tool_set) {
            (Some(skills), ToolSet::Crewmate) => Some(
                plugin::write_plugin(&self.spec.session_dir, skills).map_err(|e| {
                    StartFailure::Spawn(format!("couldn't write the skills plugin: {e}"))
                })?,
            ),
            _ => None,
        };
        self.launched_window = self.autocompact_window();
        let options = LaunchOptions::for_session(
            &self.spec,
            &self.model,
            &self.conversation,
            resuming,
            plugin_dir,
            self.launched_window,
        );
        let env = build_env(&self.shared.config.inherited_env, &self.spec)
            .map_err(|e| StartFailure::Spawn(e.to_string()))?;
        let spawn = SpawnSpec {
            program: self.shared.config.claude.clone(),
            args: build_argv(&options),
            cwd: self.spec.cwd.clone(),
            env,
            owner: ProcessOwner::Agent {
                agent: self.spec.agent,
            },
        };
        let child = self
            .shared
            .config
            .supervisor
            .spawn(spawn)
            .map_err(|e| StartFailure::Spawn(e.to_string()))?;
        self.proc = Some(Proc::attach(child));
        self.down = false;
        self.spent = saved;
        self.unasked = false;
        self.lifecycle_seen = false;

        // initialize, with our prompt and our tool server.
        let init_id = self.request_id();
        let (system_prompt, append) = match &self.spec.prompt {
            SystemPrompt::Replace { text } => (Some(text.as_str()), None),
            SystemPrompt::AppendToClaudeCode { text } => (None, Some(text.as_str())),
        };
        let message = protocol::initialize(
            &init_id,
            SERVER_NAME,
            Some(self.mcp.manifests()),
            system_prompt,
            append,
        );
        self.write(&message);
        let init = self
            .await_response(&init_id, INITIALIZE_TIMEOUT)
            .await?
            .map_err(StartFailure::Refused)?;
        if let Some(models) = init.get("models").and_then(Value::as_array) {
            self.shared.set_models(models.clone());
        }

        let context = self
            .query(json!({"subtype": "get_context_usage", "detail": "full"}))
            .await?;
        self.check_autocompact(&context);
        let hooks = self.query(json!({"subtype": "get_hooks_listing"})).await?;
        let report = self.init_report(init, context, hooks);
        if report.raw.get("tools_source").is_none()
            && let Err(problem) = crate::check::check_tools(&report.tools, &self.expected)
        {
            return Err(StartFailure::Untrusted(problem));
        }
        Ok(report)
    }

    /// The autocompact window in force in the settings now.
    fn autocompact_window(&self) -> u32 {
        u32::from(self.settings.borrow().compaction.autocompact_window)
    }

    /// Logs the autocompact window and trigger point `claude` reports at launch, and warns when
    /// it isn't ours (see [`autocompact::check`]).
    fn check_autocompact(&self, context: &Result<Value, String>) {
        let Ok(context) = context else {
            tracing::warn!(session = %self.conversation, "couldn't read claude's context usage, so its autocompact window is unchecked");
            return;
        };
        let trigger = context.get("autoCompactThreshold").and_then(Value::as_u64);
        match autocompact::check(context) {
            Ok(()) => tracing::info!(
                session = %self.conversation,
                window = self.launched_window,
                trigger,
                "claude's autocompact window comes from our settings"
            ),
            Err(problem) => tracing::warn!(
                session = %self.conversation,
                window = self.launched_window,
                trigger,
                "{problem}, so the autocompact window may not be in force"
            ),
        }
    }

    /// Sends one zero-token query and waits for its answer.
    async fn query(&mut self, request: Value) -> Result<Result<Value, String>, StartFailure> {
        let id = self.request_id();
        self.write(&protocol::control_request(&id, request));
        self.await_response(&id, QUERY_TIMEOUT)
            .await
            .or_else(|e| match e {
                // An older claude may never answer an unknown query: fall back as if refused.
                StartFailure::Timeout => Ok(Err("no answer".into())),
                other => Err(other),
            })
    }

    /// Reads until the answer to our control request `id`, serving `claude`'s own requests
    /// meanwhile.
    async fn await_response(
        &mut self,
        id: &str,
        timeout: Duration,
    ) -> Result<Result<Value, String>, StartFailure> {
        self.start_deadline = Some(self.clock.instant() + timeout);
        loop {
            match self.wait().await {
                Wake::Line(line) => {
                    let Some((raw, incoming)) = protocol::classify(&line) else {
                        continue;
                    };
                    if let Incoming::ControlResponse { request_id, result } = &incoming
                        && request_id == id
                    {
                        self.start_deadline = None;
                        return Ok(result.clone());
                    }
                    self.on_message(raw, incoming).await;
                }
                Wake::LinesClosed => {
                    if let Some(proc) = &mut self.proc {
                        proc.lines_open = false;
                    }
                }
                Wake::Exited(cause) => {
                    let stderr = match &mut self.proc {
                        Some(proc) => {
                            proc.exited = Some(cause);
                            proc.stderr_settled(&*self.clock).await
                        }
                        None => String::new(),
                    };
                    return Err(StartFailure::Exited {
                        cause: Some(cause),
                        stderr,
                    });
                }
                Wake::Timer(Timer::Start) => return Err(StartFailure::Timeout),
                Wake::Timer(_) | Wake::Command(_) | Wake::Settings(_) => {}
            }
        }
    }

    fn init_report(
        &mut self,
        mut init: Value,
        context: Result<Value, String>,
        hooks: Result<Value, String>,
    ) -> InitReport {
        redact_accounts(&mut init);
        let mut raw = serde_json::Map::new();
        let capabilities: Vec<String> = init
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let capabilities_deferred = REQUIRED_CAPABILITIES
            .iter()
            .any(|c| !capabilities.iter().any(|have| have == c));
        let tools_from_context = context.as_ref().ok().and_then(context_tools);
        let tools = tools_from_context
            .clone()
            .unwrap_or_else(|| self.mcp.qualified_names());
        if tools_from_context.is_none() {
            raw.insert("tools_source".into(), json!("mcp_only"));
        }
        if capabilities_deferred {
            // claude 2.1.303 advertises protocol capabilities only in `system/init`, which comes
            // with the first turn: they are checked there (see `check::check`).
            raw.insert("capabilities_deferred".into(), json!(true));
        }
        let permission_mode = str_at(&init, "/current_permission_mode").map(str::to_owned);
        self.init_checks = Some(InitChecks {
            capabilities: capabilities_deferred,
            permission_mode: permission_mode.clone(),
        });
        let engine_version = str_at(&init, "/claude_code_version").map(str::to_owned);
        let mut context = context.unwrap_or_else(|e| json!({ "error": e }));
        redact_accounts(&mut context);
        let mut hooks = hooks.unwrap_or_else(|e| json!({ "error": e }));
        redact_accounts(&mut hooks);
        raw.insert("initialize".into(), init);
        raw.insert("context_usage".into(), context);
        raw.insert("hooks".into(), hooks);
        InitReport {
            engine_version,
            model: ModelId::new(self.model.clone()).ok(),
            tools,
            permission_mode,
            capabilities,
            raw: Value::Object(raw),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Commands
    // ---------------------------------------------------------------------------------------

    async fn on_command(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::StartTurn { input } => {
                if self.turn.is_some() {
                    // One turn at a time: the core shouldn't send this; deliver it into the running
                    // turn rather than lose it.
                    tracing::warn!("StartTurn arrived during a turn; delivering it as an inject");
                    self.write_user(&input);
                } else {
                    self.begin_turn(input).await;
                }
            }
            SessionCommand::Inject { input } => {
                if self.turn.is_some() {
                    // claude queues it into the running turn.
                    self.write_user(&input);
                } else {
                    self.begin_turn(input).await;
                }
            }
            SessionCommand::ToolResult { call, result } => {
                if let Some(pending) = self.pending_calls.remove(call.as_str()) {
                    let response = json!({
                        "mcp_response": McpServer::call_result(&pending.rpc_id, &result)
                    });
                    self.write(&protocol::control_success(&pending.request_id, response));
                } else {
                    tracing::debug!(
                        call = call.as_str(),
                        "dropped a result for an unknown or late tool call"
                    );
                }
            }
            SessionCommand::Interrupt => {
                if self.turn.is_some() {
                    self.send_interrupt();
                }
            }
            SessionCommand::SetModel { model } => {
                model.as_str().clone_into(&mut self.model);
                if self.proc.is_some() && self.turn.is_none() {
                    self.write_set_model(model.as_str());
                } else if self.proc.is_some() {
                    self.pending_model = Some(model.as_str().to_owned());
                }
            }
            SessionCommand::Compact => {
                if self.turn.is_some() {
                    // The core sends it only between turns.
                    tracing::warn!("Compact arrived during a turn; ignored");
                } else {
                    // A user message that is only `/compact` runs `claude`'s own compaction: a
                    // turn with a `compact_boundary` and a `result`, and no model text.
                    self.begin_turn(TurnInput::text(COMPACT_COMMAND)).await;
                }
            }
            SessionCommand::Close | SessionCommand::Forget => self.on_close().await,
            other => tracing::debug!(?other, "ignored an unknown command"),
        }
    }

    async fn begin_turn(&mut self, input: TurnInput) {
        self.idle_deadline = None;
        if let Some(message) = self.refused.clone() {
            self.fail_at_once(FailureKind::Protocol, message).await;
            return;
        }
        // A change that waited for a job to finish takes effect here, before the turn.
        self.restart_for_settings().await;
        if self.proc.is_none() {
            // Down (suspended, crashed or interrupted hard): bring the session back first.
            match self.start(true).await {
                Ok(init) => {
                    self.send(EngineEvent::SessionReady {
                        session: self.session_ref(),
                        resumed: true,
                        init,
                    })
                    .await;
                }
                Err(failure) => {
                    let message = format!(
                        "couldn't resume the Claude session {}: {}",
                        self.conversation,
                        failure.sentence()
                    );
                    self.send(EngineEvent::SessionClosed {
                        reason: CloseReason::ResumeFailed { message },
                    })
                    .await;
                    self.done = true;
                    return;
                }
            }
        }
        if let Some(model) = self.pending_model.take() {
            self.write_set_model(&model);
        }
        let now = self.clock.instant();
        match protocol::user_message(&input, &uuid::Uuid::new_v4().to_string()) {
            Ok(message) => {
                self.write(&message);
                self.turn = Some(Turn::new(now));
                self.send(EngineEvent::TurnStarted).await;
            }
            Err(error) => {
                // The input couldn't be built (an unreadable image): a turn that fails at once.
                self.fail_at_once(FailureKind::Internal, error).await;
            }
        }
    }

    /// A turn that fails before anything reaches `claude`.
    async fn fail_at_once(&mut self, kind: FailureKind, message: String) {
        self.turn = Some(Turn::new(self.clock.instant()));
        self.send(EngineEvent::TurnStarted).await;
        self.turn = None;
        self.send(EngineEvent::TurnEnded {
            outcome: TurnOutcome::Failed {
                failure: TurnFailure { kind, message },
            },
            usage: qm_engine::TurnUsage::default(),
            final_text: String::new(),
        })
        .await;
        self.arm_idle();
    }

    /// A message into the running turn (an `Inject`): `claude` takes it at the next tool
    /// result, or answers it in another query after the turn's `result`; the turn waits for it.
    fn write_user(&mut self, input: &TurnInput) {
        let uuid = uuid::Uuid::new_v4().to_string();
        match protocol::user_message(input, &uuid) {
            Ok(message) => {
                self.write(&message);
                if let Some(turn) = &mut self.turn {
                    turn.injects.insert(uuid, false);
                }
            }
            Err(error) => tracing::warn!("couldn't deliver an injected message: {error}"),
        }
    }

    /// `command_lifecycle`: `claude` queued, started or completed one of our messages.
    fn on_command_lifecycle(&mut self, msg: &Value) {
        self.lifecycle_seen = true;
        let (Some(uuid), Some(state)) = (str_at(msg, "/command_uuid"), str_at(msg, "/state"))
        else {
            return;
        };
        if matches!(state, "started" | "completed")
            && let Some(turn) = &mut self.turn
            && let Some(started) = turn.injects.get_mut(uuid)
        {
            *started = true;
        }
    }

    fn send_interrupt(&mut self) {
        let deadline = self.clock.instant() + INTERRUPT_TIMEOUT;
        let Some(turn) = &mut self.turn else { return };
        if turn.interrupted {
            return;
        }
        turn.interrupted = true;
        turn.interrupt_deadline = Some(deadline);
        let id = self.request_id();
        self.write(&protocol::control_request(
            &id,
            json!({"subtype": "interrupt"}),
        ));
    }

    fn write_set_model(&mut self, model: &str) {
        let id = self.request_id();
        self.write(&protocol::control_request(
            &id,
            json!({"subtype": "set_model", "model": model}),
        ));
    }

    async fn on_close(&mut self) {
        self.closing = true;
        match &self.turn {
            Some(turn) if !turn.interrupted => self.send_interrupt(),
            Some(_) => {}
            None => self.finish(CloseReason::Requested).await,
        }
    }

    /// Ends the session: stop the process, report running jobs, `SessionClosed`.
    async fn finish(&mut self, reason: CloseReason) {
        if self.proc.is_some() {
            self.stop_process().await;
        }
        for event in self.jobs.finish_all() {
            self.send(event).await;
        }
        self.send(EngineEvent::SessionClosed { reason }).await;
        self.done = true;
    }

    // ---------------------------------------------------------------------------------------
    // Timers, exits
    // ---------------------------------------------------------------------------------------

    async fn on_timer(&mut self, timer: Timer) {
        let now = self.clock.instant();
        match timer {
            Timer::Start => {}
            Timer::Idle => {
                self.idle_deadline = None;
                if self.turn.is_none() && self.jobs.is_idle() && self.proc.is_some() {
                    self.stop_process().await;
                    self.down = true;
                    self.send(EngineEvent::Suspended).await;
                }
            }
            Timer::Activity => {
                if let Some(turn) = &self.turn
                    && self.gate.due(now)
                {
                    let kind = if turn.builtin_running.is_empty() {
                        ActivityKind::Thinking
                    } else {
                        ActivityKind::BuiltinTool
                    };
                    self.send(EngineEvent::Activity { kind }).await;
                } else {
                    self.last_event_at = now;
                }
            }
            Timer::Interrupt => {
                // No `result` came after our interrupt: stop the process, keep the session.
                if self.turn.is_none() {
                    return;
                }
                self.stop_process().await;
                for event in self.jobs.finish_all() {
                    self.send(event).await;
                }
                let turn = self.turn.take();
                let failure = turn.as_ref().and_then(|t| t.failure.clone());
                let outcome = failure.map_or(TurnOutcome::Interrupted, |failure| {
                    TurnOutcome::Failed { failure }
                });
                self.end_turn_events(turn, outcome, None).await;
                if self.ending {
                    self.send(EngineEvent::SessionClosed {
                        reason: CloseReason::EndConversation,
                    })
                    .await;
                    self.done = true;
                } else if self.closing {
                    self.finish(CloseReason::Requested).await;
                } else {
                    self.down = true;
                    self.send(EngineEvent::Suspended).await;
                }
            }
        }
    }

    /// An exit: requested stops never get here (the process is taken out first), so this is a
    /// crash.
    async fn on_exit(&mut self, cause: ExitCause) {
        if let Some(proc) = &mut self.proc {
            proc.exited = Some(cause);
        }
        // Drain what claude wrote before dying, for at most 200 ms; never wait on a pipe after
        // the leader exits beyond that.
        let deadline = self.clock.instant() + CRASH_DRAIN;
        while let Some(proc) = &mut self.proc {
            if !proc.lines_open {
                break;
            }
            let clock = Arc::clone(&self.clock);
            let line = tokio::select! {
                line = proc.lines.recv() => line,
                () = clock.sleep_until(deadline) => break,
            };
            match line {
                Some(line) => self.on_line(&line).await,
                None => {
                    if let Some(proc) = &mut self.proc {
                        proc.lines_open = false;
                    }
                }
            }
            if self.done {
                return;
            }
        }
        let stderr = match &mut self.proc {
            Some(proc) => proc.stderr_settled(&*self.clock).await,
            None => String::new(),
        };
        // Release the rest of the group in the background: a member may outlive the leader.
        if let Some(proc) = self.proc.take() {
            self.release_crashed(proc);
        }
        self.down = true;
        self.pending_calls.clear();
        self.idle_deadline = None;

        let mut message = format!("claude exited ({})", describe_exit(Some(cause)));
        if let Some(line) = last_line(&stderr) {
            message.push_str(": ");
            message.push_str(line);
        }
        for event in self.jobs.finish_all() {
            self.send(event).await;
        }
        if let Some(turn) = self.turn.take() {
            let outcome = TurnOutcome::Failed {
                failure: TurnFailure {
                    kind: FailureKind::ProcessExited,
                    message,
                },
            };
            self.end_turn_events(Some(turn), outcome, None).await;
            if self.ending {
                self.send(EngineEvent::SessionClosed {
                    reason: CloseReason::EndConversation,
                })
                .await;
                self.done = true;
            } else if self.closing {
                self.send(EngineEvent::SessionClosed {
                    reason: CloseReason::Requested,
                })
                .await;
                self.done = true;
            }
        } else if self.ending {
            self.send(EngineEvent::SessionClosed {
                reason: CloseReason::EndConversation,
            })
            .await;
            self.done = true;
        } else {
            tracing::warn!(session = %self.conversation, "{message}");
            self.send(EngineEvent::Suspended).await;
        }
    }

    fn release_crashed(&self, mut proc: Proc) {
        drop(proc.writer.take());
        let supervisor = Arc::clone(&self.shared.config.supervisor);
        let clock = Arc::clone(&self.clock);
        tokio::spawn(async move {
            let _ = stop(
                &*supervisor,
                &*clock,
                &mut proc.child,
                StopPolicy::default(),
            )
            .await;
        });
    }

    /// Stops the process's whole group: stdin closed, then SIGTERM, then SIGKILL.
    async fn stop_process(&mut self) {
        let Some(mut proc) = self.proc.take() else {
            return;
        };
        // Closing our writer closes stdin once what was queued is written.
        drop(proc.writer.take());
        let supervisor = Arc::clone(&self.shared.config.supervisor);
        match stop(
            &*supervisor,
            &*self.clock,
            &mut proc.child,
            StopPolicy::default(),
        )
        .await
        {
            Ok(report) if report.survivors => {
                tracing::warn!(group = %report.group, "members of claude's process group survived SIGKILL");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("couldn't stop claude's process group: {e}"),
        }
        self.pending_calls.clear();
    }

    // ---------------------------------------------------------------------------------------
    // Incoming
    // ---------------------------------------------------------------------------------------

    async fn on_line(&mut self, line: &[u8]) {
        let Some((raw, incoming)) = protocol::classify(line) else {
            if !line.trim_ascii().is_empty() {
                tracing::debug!("ignored a stdout line that isn't a JSON object");
            }
            return;
        };
        self.on_message(raw, incoming).await;
    }

    async fn on_message(&mut self, raw: Value, incoming: Incoming) {
        match incoming {
            Incoming::ControlResponse { .. } => {}
            Incoming::ControlRequest {
                request_id,
                request,
            } => self.on_control_request(request_id, &request).await,
            Incoming::ControlCancel { request_id } => {
                self.pending_calls
                    .retain(|_, p| p.request_id.as_str() != Some(request_id.as_str()));
            }
            Incoming::System { subtype, msg } => self.on_system(&subtype, &msg).await,
            Incoming::StreamEvent { event, parent } => {
                if parent.is_some() {
                    self.activity(ActivityKind::BuiltinTool).await;
                } else {
                    self.on_stream_event(&event).await;
                }
            }
            Incoming::Assistant { message, parent } => {
                if parent.is_some() {
                    self.activity(ActivityKind::BuiltinTool).await;
                } else {
                    self.on_assistant(&message).await;
                }
            }
            Incoming::User { msg, .. } => {
                if let (Some(turn), Some(content)) = (
                    &mut self.turn,
                    msg.pointer("/message/content").and_then(Value::as_array),
                ) {
                    for block in content {
                        if let Some(id) = str_at(block, "/tool_use_id") {
                            turn.builtin_running.remove(id);
                        }
                    }
                }
            }
            Incoming::Result(msg) => self.on_result(&msg).await,
            Incoming::RateLimit(msg) => self.on_rate_limit(&msg).await,
            Incoming::ToolProgress(_) => self.activity(ActivityKind::BuiltinTool).await,
            Incoming::Other(kind) if kind == "command_lifecycle" => {
                self.on_command_lifecycle(&raw);
            }
            Incoming::Other(kind) => {
                tracing::debug!(kind, "ignored an unknown message type");
                drop(raw);
            }
        }
    }

    async fn on_control_request(&mut self, request_id: Value, request: &Value) {
        let subtype = str_at(request, "/subtype").unwrap_or_default();
        if subtype != "mcp_message" {
            let error = format!("Unsupported control request subtype: {subtype}");
            self.write(&protocol::control_error(&request_id, &error));
            return;
        }
        let server = str_at(request, "/server_name").unwrap_or_default();
        if server != SERVER_NAME {
            let error = format!("SDK MCP server not found: {server}");
            self.write(&protocol::control_error(&request_id, &error));
            return;
        }
        let message = request.get("message").cloned().unwrap_or(Value::Null);
        match self.mcp.handle(&message) {
            McpOutcome::Reply(reply) => {
                self.write(&protocol::control_success(
                    &request_id,
                    json!({ "mcp_response": reply }),
                ));
            }
            McpOutcome::Call {
                id,
                name,
                args,
                tool_use_id,
            } => {
                if self.turn.is_none() || self.starting {
                    let reply = McpServer::call_result(
                        &id,
                        &qm_engine::ToolResult::error("no turn is running, so the tool can't run"),
                    );
                    self.write(&protocol::control_success(
                        &request_id,
                        json!({ "mcp_response": reply }),
                    ));
                    return;
                }
                let call = tool_use_id.unwrap_or_else(|| match &request_id {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                self.pending_calls.insert(
                    call.clone(),
                    PendingCall {
                        request_id,
                        rpc_id: id,
                    },
                );
                self.send(EngineEvent::ToolCall {
                    call: ToolCallId::new(call),
                    name,
                    args,
                })
                .await;
            }
        }
    }

    async fn on_system(&mut self, subtype: &str, msg: &Value) {
        match subtype {
            "status" => {
                if str_at(msg, "/status") == Some("requesting") {
                    self.call_started().await;
                }
            }
            "session_state_changed" if str_at(msg, "/state") == Some("running") => {
                if self.turn.is_none()
                    && !self.starting
                    && !self.closing
                    && !self.ending
                    && self.proc.is_some()
                {
                    // Claude started a turn by itself (e.g. after a background job): report it
                    // as a turn.
                    tracing::info!(session = %self.conversation, "claude started a turn nobody requested");
                    self.idle_deadline = None;
                    self.turn = Some(Turn::new(self.clock.instant()));
                    self.send(EngineEvent::TurnStarted).await;
                }
            }
            "init" => self.on_system_init(msg).await,
            "api_retry" => {
                if self.turn.is_some() {
                    let (attempt, reported_max, error, delay_ms) = stream::api_retry(msg);
                    let max_attempts = self
                        .spec
                        .limits
                        .map(|l| l.max_retries)
                        .or(reported_max)
                        .unwrap_or(attempt);
                    self.send(EngineEvent::Retrying {
                        attempt,
                        max_attempts,
                        error,
                        delay_ms,
                    })
                    .await;
                }
            }
            "compact_boundary" => {
                let (automatic, tokens_before, tokens_after) = stream::compacted(msg);
                self.send(EngineEvent::Compacted {
                    automatic,
                    tokens_before,
                    tokens_after,
                })
                .await;
            }
            "task_started" => {
                if self.turn.is_some() {
                    if let Some(event) = self.jobs.started(msg, self.clock.instant()) {
                        self.idle_deadline = None;
                        self.send(event).await;
                    }
                } else {
                    tracing::warn!(
                        "a background task started between turns; it isn't tracked as a job"
                    );
                }
            }
            "task_notification" | "task_updated" => {
                if let Some(event) = self.jobs.notification(msg) {
                    self.send(event).await;
                    if self.turn.is_none() {
                        self.arm_idle();
                    }
                }
            }
            "thinking_tokens" => self.activity(ActivityKind::Thinking).await,
            other => tracing::debug!(subtype = other, "ignored a system message"),
        }
    }

    async fn on_system_init(&mut self, msg: &Value) {
        let mut redacted = msg.clone();
        redact_accounts(&mut redacted);
        self.shared.set_system_init(redacted);
        if self.turn.is_none() && !self.starting && self.proc.is_some() {
            // `claude` started a turn by itself, on a background task's notification. Turns are the
            // app's: stop it before the model acts. The notification stays in the conversation, the
            // finished job wakes the crewmate through the app, and what the stopped turn spent
            // counts in the next turn (`spent`).
            tracing::info!(session = %self.conversation, "interrupting a turn claude started by itself");
            self.unasked = true;
            let id = self.request_id();
            self.write(&protocol::control_request(
                &id,
                json!({"subtype": "interrupt"}),
            ));
        }
        let mut problems = Vec::new();
        // The tools at every `system/init`: only what the role is granted, ever.
        let tools: Vec<String> = msg
            .get("tools")
            .and_then(Value::as_array)
            .map(|t| {
                t.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if let Err(problem) = crate::check::check_tools(&tools, &self.expected) {
            problems.push(problem);
        }
        if let Some(checks) = self.init_checks.take() {
            self.first_init_problems(msg, &checks, &mut problems);
        }
        if problems.is_empty() {
            return;
        }
        let message = problems.join("; ");
        tracing::warn!(session = %self.conversation, "refusing the session: {message}");
        self.refuse(message).await;
    }

    /// Stops the process before the model can act and fails the turn; every later turn fails at
    /// once ([`Self::begin_turn`]).
    async fn refuse(&mut self, message: String) {
        self.refused = Some(message.clone());
        self.stop_process().await;
        self.down = true;
        self.idle_deadline = None;
        for event in self.jobs.finish_all() {
            self.send(event).await;
        }
        if let Some(turn) = self.turn.take() {
            let outcome = TurnOutcome::Failed {
                failure: TurnFailure {
                    kind: FailureKind::Protocol,
                    message,
                },
            };
            self.end_turn_events(Some(turn), outcome, None).await;
        }
        if self.closing || self.ending {
            self.finish(CloseReason::Requested).await;
        }
    }

    /// The checks only the first `system/init` makes.
    fn first_init_problems(&self, msg: &Value, checks: &InitChecks, problems: &mut Vec<String>) {
        if let Some(sid) = str_at(msg, "/session_id")
            && sid != self.conversation
        {
            problems.push(format!(
                "claude reported session {sid}, but the session is {}",
                self.conversation
            ));
        }
        if let (Some(expected), Some(got)) =
            (&checks.permission_mode, str_at(msg, "/permissionMode"))
            && expected != got
        {
            problems.push(format!(
                "claude reported permission mode {got}, but initialize reported {expected}"
            ));
        }
        if checks.capabilities {
            let caps: Vec<&str> = msg
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|c| c.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for required in REQUIRED_CAPABILITIES {
                if !caps.contains(required) {
                    problems.push(format!(
                        "claude doesn't support the {required} protocol capability"
                    ));
                }
            }
        }
    }

    async fn on_stream_event(&mut self, event: &Value) {
        if self.turn.is_none() {
            return;
        }
        match str_at(event, "/type").unwrap_or_default() {
            "message_start" => {
                self.call_started().await;
                if let Some(turn) = &mut self.turn {
                    turn.blocks.clear();
                }
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let kind = str_at(event, "/content_block/type").unwrap_or_default();
                if let Some(turn) = &mut self.turn {
                    turn.blocks.insert(index, kind.to_owned());
                }
            }
            "content_block_delta" => match str_at(event, "/delta/type").unwrap_or_default() {
                "text_delta" => {
                    let text = str_at(event, "/delta/text").unwrap_or_default();
                    let lines = match &mut self.turn {
                        Some(turn) => {
                            turn.call_text.push_str(text);
                            turn.lines.push(text)
                        }
                        None => Vec::new(),
                    };
                    for text in lines {
                        self.send(EngineEvent::TextLine { text }).await;
                    }
                }
                "thinking_delta" | "input_json_delta" | "signature_delta" => {
                    self.activity(ActivityKind::Thinking).await;
                }
                _ => {}
            },
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let flushed = match &mut self.turn {
                    Some(turn) if turn.blocks.get(&index).is_some_and(|k| k == "text") => {
                        turn.lines.flush()
                    }
                    _ => None,
                };
                if let Some(text) = flushed {
                    self.send(EngineEvent::TextLine { text }).await;
                }
            }
            "message_delta" => {
                if let Some(turn) = &mut self.turn {
                    if let Some(usage) = event.get("usage") {
                        turn.call_usage = stream::call_usage(usage);
                    }
                    if let Some(reason) = str_at(event, "/delta/stop_reason") {
                        turn.call_stop = stream::stop_reason(Some(reason));
                    }
                }
            }
            "message_stop" => self.call_finished().await,
            _ => {}
        }
    }

    async fn call_started(&mut self) {
        let call = match &mut self.turn {
            Some(turn) if !turn.call_open => {
                turn.calls += 1;
                turn.call_open = true;
                turn.call_usage = CallUsage::default();
                turn.call_stop = StopReason::Unknown;
                turn.call_text.clear();
                turn.calls
            }
            _ => return,
        };
        self.send(EngineEvent::ModelCallStarted { call }).await;
    }

    async fn call_finished(&mut self) {
        let cap = self.spec.limits.map(|l| l.max_output_tokens);
        let (call, usage, stop, over_cap) = match &mut self.turn {
            Some(turn) if turn.call_open => {
                turn.call_open = false;
                let u = turn.call_usage;
                turn.spent.input_tokens += u.input_tokens;
                turn.spent.output_tokens += u.output_tokens;
                turn.spent.thinking_tokens += u.thinking_tokens;
                turn.spent.cache_read_tokens += u.cache_read_tokens;
                turn.spent.cache_write_tokens += u.cache_write_tokens;
                // A partial last line of an unterminated text block.
                let over_cap = cap.is_some_and(|cap| {
                    turn.call_usage.output_tokens > u64::from(cap)
                        || turn.call_stop == StopReason::MaxTokens
                });
                (turn.calls, turn.call_usage, turn.call_stop, over_cap)
            }
            _ => return,
        };
        if let Some(text) = self.turn.as_mut().and_then(|t| t.lines.flush()) {
            self.send(EngineEvent::TextLine { text }).await;
        }
        self.send(EngineEvent::ModelCallFinished { call, usage, stop })
            .await;
        if over_cap && let Some(cap) = cap {
            if let Some(turn) = &mut self.turn {
                turn.failure.get_or_insert(TurnFailure {
                    kind: FailureKind::Provider,
                    message: stream::output_cap_message(cap),
                });
            }
            self.send_interrupt();
        }
    }

    async fn on_assistant(&mut self, message: &Value) {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        if self.turn.is_none() {
            return;
        }
        let mut out = Vec::new();
        for block in content {
            if str_at(block, "/type") != Some("tool_use") {
                continue;
            }
            let id = str_at(block, "/id").unwrap_or_default().to_owned();
            let name = str_at(block, "/name").unwrap_or_default().to_owned();
            let Some(turn) = &mut self.turn else { break };
            if !id.is_empty() && !turn.seen_tool_uses.insert(id.clone()) {
                continue;
            }
            if name.starts_with(&format!("mcp__{SERVER_NAME}__")) {
                // The `mcp_message` that follows makes the ToolCall.
                continue;
            }
            if name == "EndConversation" {
                if !self.ending {
                    self.ending = true;
                    out.push(EngineEvent::EndConversationCalled);
                }
                continue;
            }
            if name == "Bash" && !id.is_empty() {
                self.jobs.saw_bash_call(&id, self.clock.instant());
            }
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            if !id.is_empty() {
                turn.builtin_running.insert(id);
            }
            out.push(EngineEvent::BuiltinToolUse {
                summary: stream::builtin_summary(&name, &input),
                name,
            });
        }
        for event in out {
            self.send(event).await;
        }
    }

    async fn on_result(&mut self, msg: &Value) {
        let Some(mut turn) = self.turn.take() else {
            // The end of a turn `claude` started by itself; its spend counts in the next turn.
            tracing::debug!(unasked = self.unasked, "ignored a result outside a turn");
            self.unasked = false;
            self.restart_for_settings().await;
            return;
        };
        if let Some(text) = turn.lines.flush() {
            self.turn = Some(turn);
            self.send(EngineEvent::TextLine { text }).await;
            turn = self
                .turn
                .take()
                .unwrap_or_else(|| Turn::new(self.clock.instant()));
        }
        let is_error = msg.get("is_error").and_then(Value::as_bool) == Some(true)
            || str_at(msg, "/subtype").is_some_and(|s| s != "success");
        if !is_error
            && self.lifecycle_seen
            && turn.failure.is_none()
            && !turn.interrupted
            && turn.injects.values().any(|started| !started)
        {
            // A message sent mid-turn is still queued: `claude` answers it in another query now,
            // and the turn goes on until that one's `result` (its usage is counted then too).
            tracing::debug!(session = %self.conversation, "a result with a message still queued; the turn goes on");
            turn.call_text.clear();
            self.turn = Some(turn);
            return;
        }
        let outcome = if let Some(failure) = turn.failure.clone() {
            TurnOutcome::Failed { failure }
        } else if turn.interrupted {
            TurnOutcome::Interrupted
        } else if !is_error {
            TurnOutcome::Completed
        } else if turn.rate_limited
            || msg.get("api_error_status").and_then(Value::as_u64) == Some(429)
        {
            TurnOutcome::Failed {
                failure: TurnFailure {
                    kind: FailureKind::AccountLimit,
                    message: stream::result_error(msg),
                },
            }
        } else {
            TurnOutcome::Failed {
                failure: TurnFailure {
                    kind: FailureKind::Provider,
                    message: stream::result_error(msg),
                },
            }
        };
        self.end_turn_events(Some(turn), outcome, Some(msg)).await;

        if self.ending {
            self.finish(CloseReason::EndConversation).await;
        } else if self.closing {
            self.finish(CloseReason::Requested).await;
        } else {
            if let Some(model) = self.pending_model.take() {
                self.write_set_model(&model);
            }
            self.arm_idle();
            self.restart_for_settings().await;
        }
    }

    /// Sends `TurnEnded` for `turn` (already taken out of `self.turn`).
    async fn end_turn_events(
        &mut self,
        turn: Option<Turn>,
        outcome: TurnOutcome,
        result: Option<&Value>,
    ) {
        let now = self.clock.instant();
        let (calls, started, call_text, spent) = turn
            .map_or((0, now, String::new(), CallUsage::default()), |t| {
                (t.calls, t.started, t.call_text, t.spent)
            });
        let duration_ms =
            u64::try_from(now.saturating_duration_since(started).as_millis()).unwrap_or(u64::MAX);
        let (usage, final_text) = match result {
            Some(result) => {
                let (usage, spent) = stream::turn_usage(result, calls, duration_ms, &self.spent);
                self.spent = spent;
                (
                    usage,
                    str_at(result, "/result").map_or(call_text, str::to_owned),
                )
            }
            // No `result` (a crash, a lost process): what the finished calls reported.
            None => (
                qm_engine::TurnUsage {
                    model_calls: calls,
                    input_tokens: spent.input_tokens,
                    output_tokens: spent.output_tokens,
                    thinking_tokens: spent.thinking_tokens,
                    cache_read_tokens: spent.cache_read_tokens,
                    cache_write_tokens: spent.cache_write_tokens,
                    cost_usd: None,
                    duration_ms,
                },
                call_text,
            ),
        };
        self.pending_calls.clear();
        self.send(EngineEvent::TurnEnded {
            outcome,
            usage,
            final_text,
        })
        .await;
    }

    async fn on_rate_limit(&mut self, msg: &Value) {
        if let Some(reading) = stream::usage_reading(msg, self.clock.now()) {
            self.shared.set_usage(reading);
        }
        let Some(info) = stream::rate_limit(msg) else {
            return;
        };
        if info.status == qm_engine::RateLimitStatus::Rejected
            && let Some(turn) = &mut self.turn
        {
            turn.rate_limited = true;
        }
        self.send(EngineEvent::RateLimit { info }).await;
    }

    /// Silent work: an `Activity` if the gate says it is due (only inside a turn).
    async fn activity(&mut self, kind: ActivityKind) {
        if self.turn.is_none() || self.starting {
            return;
        }
        let now = self.clock.instant();
        if self.gate.due(now) {
            self.send(EngineEvent::Activity { kind }).await;
        }
    }

    /// Restarts a running `claude` that launched with another autocompact window, when it is idle
    /// with no job: the process stops as at an idle release, and the next turn resumes it with
    /// the new `--settings`. A turn or a job holds the restart (see [`autocompact::restart`]).
    async fn restart_for_settings(&mut self) {
        if self.proc.is_none() || self.starting || self.closing || self.ending || self.done {
            return;
        }
        let decision = autocompact::restart(
            self.launched_window,
            self.autocompact_window(),
            self.turn.is_some(),
            !self.jobs.is_idle(),
        );
        if decision != Restart::Now {
            return;
        }
        tracing::info!(
            session = %self.conversation,
            from = self.launched_window,
            to = self.autocompact_window(),
            "restarting claude for the new autocompact window"
        );
        self.idle_deadline = None;
        self.stop_process().await;
        self.down = true;
        self.send(EngineEvent::Suspended).await;
    }

    fn arm_idle(&mut self) {
        self.idle_deadline = match self.spec.idle_shutdown {
            Some(after) if self.turn.is_none() && self.jobs.is_idle() && self.proc.is_some() => {
                Some(self.clock.instant() + after)
            }
            _ => None,
        };
    }

    // ---------------------------------------------------------------------------------------
    // Output
    // ---------------------------------------------------------------------------------------

    /// Sends one event to the core. Nothing goes out while the process starts.
    async fn send(&mut self, event: EngineEvent) {
        if self.starting {
            tracing::debug!(event = event.name(), "dropped an event while starting");
            return;
        }
        let now = self.clock.instant();
        self.gate.sent(now);
        self.last_event_at = now;
        if self.core_gone {
            return;
        }
        if self.events.send(event).await.is_err() {
            self.core_gone = true;
        }
    }

    fn request_id(&mut self) -> String {
        self.next_request += 1;
        format!("qm-{}", self.next_request)
    }

    /// Writes one message to claude's stdin.
    fn write(&mut self, message: &Value) {
        let Some(writer) = self.proc.as_ref().and_then(|p| p.writer.as_ref()) else {
            return;
        };
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        // A closed writer means the process is going down; the exit is handled elsewhere.
        let _ = writer.send(line);
    }

    fn session_ref(&self) -> SessionRef {
        SessionRef::new(self.conversation.clone()).unwrap_or_else(|_| {
            // A UUID or a stored reference is never empty.
            SessionRef::new("unknown").expect("a non-empty reference")
        })
    }
}

impl Proc {
    /// Takes the child's pipes and starts the writer, reader and stderr tasks.
    fn attach(mut child: Child) -> Self {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        if let Some(stdin) = child.stdin.take() {
            tokio::spawn(write_stdin(stdin, writer_rx));
        }
        let (lines_tx, lines_rx) = mpsc::unbounded_channel();
        let lines_open = match child.stdout.take() {
            Some(stdout) => {
                tokio::spawn(read_lines(stdout, lines_tx));
                true
            }
            None => false,
        };
        let stderr = Arc::new(Mutex::new(String::new()));
        let (done_tx, done_rx) = watch::channel(false);
        match child.stderr.take() {
            Some(err) => {
                tokio::spawn(read_stderr(err, Arc::clone(&stderr), done_tx));
            }
            None => {
                let _ = done_tx.send(true);
            }
        }
        let exit = child.exit.clone();
        Self {
            child,
            writer: Some(writer_tx),
            lines: lines_rx,
            lines_open,
            exit,
            exited: None,
            stderr,
            stderr_done: done_rx,
        }
    }

    /// The stderr tail once stderr closed, waiting at most 200 ms for it.
    async fn stderr_settled(&mut self, clock: &dyn Clock) -> String {
        let deadline = clock.instant() + CRASH_DRAIN;
        let mut done = self.stderr_done.clone();
        tokio::select! {
            _ = done.wait_for(|d| *d) => {}
            () = clock.sleep_until(deadline) => {}
        }
        self.stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

async fn recv_line(lines: Option<&mut mpsc::UnboundedReceiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match lines {
        Some(lines) => lines.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_exit(exit: Option<&mut ExitWatch>) -> ExitCause {
    match exit {
        Some(exit) => exit.wait().await,
        None => std::future::pending().await,
    }
}

async fn write_stdin(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
    // Dropping stdin closes it: claude exits on EOF.
    let _ = stdin.shutdown().await;
}

/// Reads stdout line by line; lines longer than [`MAX_LINE`] are dropped with a warning.
async fn read_lines(stdout: ChildOutput, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut line = Vec::new();
    let mut discarding = false;
    loop {
        let (consumed, complete) = {
            let buf = match reader.fill_buf().await {
                Ok([]) | Err(_) => break,
                Ok(buf) => buf,
            };
            if let Some(end) = buf.iter().position(|&b| b == b'\n') {
                if !discarding {
                    line.extend_from_slice(&buf[..end]);
                }
                (end + 1, true)
            } else {
                if !discarding {
                    line.extend_from_slice(buf);
                }
                (buf.len(), false)
            }
        };
        reader.consume(consumed);
        if !discarding && line.len() > MAX_LINE {
            tracing::warn!("dropped a stdout line longer than 16 MiB");
            line = Vec::new();
            discarding = true;
        }
        if complete {
            if !discarding && tx.send(std::mem::take(&mut line)).is_err() {
                return;
            }
            line.clear();
            discarding = false;
        }
    }
    if !line.is_empty() && !discarding {
        let _ = tx.send(line);
    }
}

async fn read_stderr(mut err: ChildOutput, tail: Arc<Mutex<String>>, done: watch::Sender<bool>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match err.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
                tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                if tail.len() > STDERR_TAIL {
                    let mut cut = tail.len() - STDERR_TAIL;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        }
    }
    let _ = done.send(true);
}

fn describe_exit(cause: Option<ExitCause>) -> String {
    match cause {
        Some(ExitCause::Exited { code }) => format!("code {code}"),
        Some(ExitCause::Signaled { signal }) => format!("signal {signal}"),
        None => "unknown cause".into(),
    }
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

fn crashed(failure: &StartFailure) -> CloseReason {
    match failure {
        StartFailure::Exited { cause, stderr } => CloseReason::Crashed {
            exit_code: match cause {
                Some(ExitCause::Exited { code }) => Some(*code),
                _ => None,
            },
            signal: match cause {
                Some(ExitCause::Signaled { signal }) => Some(*signal),
                _ => None,
            },
            stderr_tail: stderr.clone(),
        },
        other => CloseReason::Crashed {
            exit_code: None,
            signal: None,
            stderr_tail: other.sentence(),
        },
    }
}

/// The tool list from `get_context_usage`: `systemTools[].name` and `mcp__<server>__<name>` for
/// `mcpTools`. `None` when the answer has neither list.
fn context_tools(context: &Value) -> Option<Vec<String>> {
    let system = context.get("systemTools").and_then(Value::as_array);
    let mcp = context.get("mcpTools").and_then(Value::as_array);
    if system.is_none() && mcp.is_none() {
        return None;
    }
    let mut tools = Vec::new();
    for tool in system.into_iter().flatten() {
        if let Some(name) = tool
            .as_str()
            .or_else(|| tool.get("name").and_then(Value::as_str))
        {
            tools.push(name.to_owned());
        }
    }
    for tool in mcp.into_iter().flatten() {
        let name = tool
            .as_str()
            .or_else(|| tool.get("name").and_then(Value::as_str));
        let Some(name) = name else { continue };
        if name.starts_with("mcp__") {
            tools.push(name.to_owned());
        } else {
            let server = tool
                .get("serverName")
                .or_else(|| tool.get("server"))
                .and_then(Value::as_str)
                .unwrap_or(SERVER_NAME);
            tools.push(format!("mcp__{server}__{name}"));
        }
    }
    Some(tools)
}
