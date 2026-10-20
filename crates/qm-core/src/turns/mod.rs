//! The Quartermaster's turn loop.
//!
//! One Tokio task, [`TurnLoop::run`], owns the seat's `SessionHandle` and is the only reader of
//! its events. It runs one turn at a time:
//!
//! 1. **Something arrives** ([`queue`]): your message (one turn each, queued while a turn runs), or
//!    wakes, batched within about two seconds. `Esc` interrupts the running turn.
//! 2. **The input** ([`input`]): the turn is recorded and stamped started by you or by an app
//!    event, its wakes are claimed, and the context block, the wakes and your message are
//!    assembled.
//! 3. **The engine runs it** (`engine_turn`): every tool call is dispatched with the turn's
//!    `StartedBy` in `Caller::turn`, so overrides are refused on turns you didn't start; the limits
//!    apply throughout ([`deadline`]).
//! 4. **The check** (`end`): a limit or a failure fails the turn with a plain reason; at the end
//!    of a turn you started, the effective-action rule runs, with its one retry and its notice.
//! 5. **The commit**: the turn, its events and its wakes in one transaction. Wakes are consumed
//!    only now; a crash mid-turn leaves them to be redelivered (`recover`).
//!
//! The loop reaches everything else through the seam in [`crate::turn_io`]. Time comes only from
//! the injected clock.

mod dispatch;
mod end;
mod engine_turn;
mod recover;

pub mod deadline;
pub mod effects;
pub mod input;
pub mod queue;

pub use end::fit_acknowledgement;
pub use recover::{QUIT_REASON, RERUN_USER_TURNS_AFTER_QUIT};

use crate::turn_io::{
    ChatLog, ContextSource, OpenReason, SeatError, SeatInboxRx, SeatInput, SeatSession, SeatStatus,
};
use qm_engine::{CloseReason, EngineEvent, SessionHandle};
use qm_store::db::Db;
use qm_tools::handler::{Dispatcher, ToolHost};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::Role;
use qm_types::ids::{AgentId, MessageId, ModelId, SecondmateId, TurnId};
use queue::{InputQueue, Next};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Wakes are ready once none has arrived for this long.
pub const WAKE_QUIET: Duration = Duration::from_secs(2);
/// …or once the first has waited this long.
pub const WAKE_MAX_HOLD: Duration = Duration::from_secs(10);
/// The most wakes one turn claims.
pub const CLAIM_LIMIT: u32 = 50;
/// How long the loop waits for a turn to end after an interrupt, and for a session to close,
/// before it treats the session as lost.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(15);

/// What the loop is built from.
pub struct TurnLoopDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub seat: Arc<dyn SeatSession>,
    pub context: Arc<dyn ContextSource>,
    pub chat: Arc<dyn ChatLog>,
    pub host: Arc<dyn ToolHost>,
    /// `Dispatcher::builtin()` in the app.
    pub dispatcher: Dispatcher,
}

impl fmt::Debug for TurnLoopDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnLoopDeps")
            .field("seat", &self.seat)
            .field("context", &self.context)
            .field("chat", &self.chat)
            .finish_non_exhaustive()
    }
}

/// Whose turns the loop runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopRole {
    Quartermaster,
    Secondmate(SecondmateId),
}

impl LoopRole {
    /// The role its tool calls and limits use.
    pub const fn role(self) -> Role {
        match self {
            Self::Quartermaster => Role::Quartermaster,
            Self::Secondmate(_) => Role::Secondmate,
        }
    }

    /// Whose, as messages say it: "the Quartermaster's" or "SM-01's".
    pub fn possessive(self) -> String {
        match self {
            Self::Quartermaster => "the Quartermaster's".to_owned(),
            Self::Secondmate(id) => format!("{id}'s"),
        }
    }

    /// The secondmate whose loop this is.
    pub const fn secondmate(self) -> Option<SecondmateId> {
        match self {
            Self::Quartermaster => None,
            Self::Secondmate(id) => Some(id),
        }
    }
}

/// As the log names it: `Quartermaster` or the secondmate's ID (`SM-01`).
impl fmt::Display for LoopRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quartermaster => f.write_str("Quartermaster"),
            Self::Secondmate(id) => write!(f, "{id}"),
        }
    }
}

/// The seat's live session.
#[derive(Debug)]
struct Live {
    agent: AgentId,
    handle: SessionHandle,
    /// A new session's summary, put first in its next turn's input.
    preamble: Option<String>,
    /// The model the engine reported running, for the `turns` row.
    model: Option<ModelId>,
}

/// The Quartermaster's turn loop.
pub struct TurnLoop {
    deps: TurnLoopDeps,
    inbox: SeatInboxRx,
    queue: InputQueue,
    session: Option<Live>,
    /// The seat's agent row, once known (from the store at startup, or from the session).
    agent: Option<AgentId>,
    /// Why the next `open` happens.
    next_open: OpenReason,
    /// Whether the last `open` failed, and whether that was shown.
    open: OpenTrouble,
    /// The note the next turn that takes wakes carries after a restart cut a wake turn short.
    restart_note: Option<String>,
    shutting_down: bool,
    turn_running: bool,
    running_message: Option<MessageId>,
    running_turn: Option<TurnId>,
    /// Whose turns these are.
    who: LoopRole,
    /// Open the session only when the first turn starts (a secondmate stays idle until then).
    lazy_open: bool,
}

impl fmt::Debug for TurnLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnLoop")
            .field("agent", &self.agent)
            .field("queue", &self.queue)
            .field("turn_running", &self.turn_running)
            .field("shutting_down", &self.shutting_down)
            .finish_non_exhaustive()
    }
}

/// Where opening the session stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OpenTrouble {
    /// The last `open` failed: nothing runs until the next input, which tries again.
    blocked: bool,
    /// The failure has been shown once.
    shown: bool,
}

/// What an input asks of a running turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Nothing,
    Interrupt,
    Shutdown,
}

/// What woke the loop while idle.
enum IdleWake {
    Event(Option<EngineEvent>),
    Input(Option<SeatInput>),
    Timer,
    /// A setting the seat's model follows changed.
    ModelSettings,
}

impl TurnLoop {
    pub fn new(deps: TurnLoopDeps, inbox: SeatInboxRx) -> Self {
        Self {
            deps,
            inbox,
            queue: InputQueue::new(),
            session: None,
            agent: None,
            next_open: OpenReason::Startup,
            open: OpenTrouble::default(),
            restart_note: None,
            shutting_down: false,
            turn_running: false,
            running_message: None,
            running_turn: None,
            who: LoopRole::Quartermaster,
            lazy_open: false,
        }
    }

    /// A secondmate's loop: recovers at start like the seat, but opens its session only when its
    /// first turn starts.
    pub fn for_secondmate(deps: TurnLoopDeps, inbox: SeatInboxRx, id: SecondmateId) -> Self {
        let mut this = Self::new(deps, inbox);
        this.who = LoopRole::Secondmate(id);
        this.lazy_open = true;
        this
    }

    /// The agent whose turns this loop runs, once it exists.
    async fn own_agent(&self) -> Result<Option<AgentId>, qm_store::StoreError> {
        match self.who {
            LoopRole::Quartermaster => {
                self.deps
                    .db
                    .call(|conn| qm_store::turns::seat_agent(conn))
                    .await
            }
            LoopRole::Secondmate(id) => {
                self.deps
                    .db
                    .call(move |conn| qm_store::secondmates::agent_of(conn, id))
                    .await
            }
        }
    }

    /// Recovers, opens the session (`OpenReason::Startup`), then serves until `Shutdown` or every
    /// `SeatInbox` is dropped.
    pub async fn run(mut self) -> Result<(), SeatError> {
        self.recover().await?;
        if !self.lazy_open {
            self.ensure_session().await;
        }
        loop {
            if self.shutting_down {
                self.close_session().await;
                self.publish();
                return Ok(());
            }
            // Inputs that came in while the last turn finished are taken as idle ones first, so an
            // `Esc` meant for that turn never stops the next.
            self.drain_inputs();
            if self.shutting_down {
                continue;
            }
            self.publish();
            let next = if self.open.blocked {
                Next::Idle
            } else {
                self.queue.next(self.deps.clock.instant())
            };
            match next {
                Next::User(_) => self.run_turn(engine_turn::Start::User).await?,
                // A batch of only ride-along wakes (an intervention) starts no turn: it is
                // claimed by the next turn, whatever signal closed the batch.
                Next::Wakes if !self.a_pending_wake_wakes().await? => self.queue.wakes_taken(),
                Next::Wakes => self.run_turn(engine_turn::Start::Wakes).await?,
                Next::WaitUntil(t) => self.idle(Some(t)).await?,
                Next::Idle => self.idle(None).await?,
            }
        }
    }

    /// Publishes the loop's state for the Bridge when it changed.
    fn publish(&self) {
        let status = SeatStatus {
            turn_running: self.turn_running,
            running_message: self.running_message,
            running_turn: self.running_turn,
            queued: self.queue.queued(),
        };
        self.inbox.status.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status;
                true
            }
        });
    }

    /// Takes one input into account. `None` means every sender is gone: the app is going away.
    fn take_input(&mut self, input: Option<SeatInput>) -> Control {
        // Any input is a reason to try opening the session again.
        self.open.blocked = false;
        match input {
            Some(SeatInput::User { message }) => {
                self.queue.push_user(message);
                self.publish();
                Control::Nothing
            }
            Some(SeatInput::Wake) => {
                self.queue.note_wake(self.deps.clock.instant());
                Control::Nothing
            }
            Some(SeatInput::Interrupt) => Control::Interrupt,
            Some(SeatInput::Shutdown) | None => {
                self.shutting_down = true;
                Control::Shutdown
            }
        }
    }

    /// Takes every input already waiting, as inputs between turns (`Esc` does nothing).
    fn drain_inputs(&mut self) {
        loop {
            match self.inbox.inputs.try_recv() {
                Ok(input) => {
                    self.take_input(Some(input));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.take_input(None);
                    return;
                }
            }
        }
    }

    /// Waits for the next thing to do: an input, an event from the session between turns, or the
    /// wake batch closing at `until`.
    async fn idle(&mut self, until: Option<Instant>) -> Result<(), SeatError> {
        let sleep: BoxFuture<'static, ()> = match until {
            Some(t) => self.deps.clock.sleep_until(t),
            None => Box::pin(std::future::pending()),
        };
        let wake = tokio::select! {
            biased;
            ev = next_event(self.session.as_mut()) => IdleWake::Event(ev),
            input = self.inbox.inputs.recv() => IdleWake::Input(input),
            () = sleep => IdleWake::Timer,
            () = self.deps.seat.model_settings_changed() => IdleWake::ModelSettings,
        };
        match wake {
            IdleWake::Timer => Ok(()),
            IdleWake::ModelSettings => {
                // Follow the change now, so what Settings shows as running is true at once.
                self.between_turns().await;
                self.deps.chat.changed();
                Ok(())
            }
            IdleWake::Input(input) => {
                // `Esc` while idle does nothing; `Shutdown` is handled at the top of the loop.
                self.take_input(input);
                Ok(())
            }
            IdleWake::Event(None) => {
                self.lose_session(OpenReason::Closed {
                    reason: "the engine stopped without closing the session".to_owned(),
                });
                self.ensure_session().await;
                Ok(())
            }
            IdleWake::Event(Some(event)) => self.idle_event(event).await,
        }
    }

    /// An event from the session while no turn runs.
    async fn idle_event(&mut self, event: EngineEvent) -> Result<(), SeatError> {
        match event {
            EngineEvent::SessionReady {
                session,
                resumed,
                init,
            } => self.session_ready(&session, resumed, &init).await,
            // The Claude engine may report a turn nobody asked for: it is recorded as an app-event
            // turn with no wakes and handled like any other.
            EngineEvent::TurnStarted => self.run_turn(engine_turn::Start::Adopted).await?,
            EngineEvent::SessionClosed { reason } => {
                let why = match reason {
                    CloseReason::ResumeFailed { message } => OpenReason::ResumeFailed { message },
                    other => OpenReason::Closed {
                        reason: close_reason(&other),
                    },
                };
                self.lose_session(why);
                self.ensure_session().await;
            }
            EngineEvent::Compacted {
                automatic,
                tokens_before,
                tokens_after,
            } => {
                self.record_compacted(None, automatic, tokens_before, tokens_after)
                    .await?;
            }
            EngineEvent::RateLimit { info } => {
                tracing::info!(status = ?info.status, resets_at = ?info.resets_at, "seat rate limit report");
            }
            // The seat never sets `idle_shutdown`; if the engine suspends anyway, the next
            // `StartTurn` brings the session back (rule 4 of `qm_engine::EngineEvent`'s order).
            EngineEvent::Suspended => tracing::debug!("the seat's session was suspended"),
            other => tracing::debug!(event = other.name(), "seat event between turns ignored"),
        }
        Ok(())
    }

    /// The engine reported its session ready.
    async fn session_ready(
        &mut self,
        session: &qm_types::ids::SessionRef,
        resumed: bool,
        init: &qm_engine::InitReport,
    ) {
        let Some(live) = self.session.as_mut() else {
            return;
        };
        if init.model.is_some() {
            live.model.clone_from(&init.model);
        }
        let agent = live.agent;
        if let Err(e) = self
            .deps
            .seat
            .session_ready(agent, session, resumed, init)
            .await
        {
            tracing::warn!(error = %e, "couldn't record the seat's session");
        }
    }

    /// Records `seat.compacted`.
    async fn record_compacted(
        &self,
        turn: Option<TurnId>,
        automatic: bool,
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
    ) -> Result<(), SeatError> {
        let Some(agent) = self.agent else {
            return Ok(());
        };
        let event = qm_store::turns::SeatEvent {
            kind: qm_store::turns::kind::COMPACTED,
            agent,
            turn,
            summary: format!("{} conversation was compacted", self.who.possessive()),
            payload: serde_json::json!({
                "automatic": automatic,
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
            }),
            at: self.deps.clock.now(),
        };
        self.deps
            .db
            .call(move |conn| qm_store::turns::record_event(conn, &event))
            .await?;
        Ok(())
    }

    /// Makes sure a session is open; `false` if none could be.
    async fn ensure_session(&mut self) -> bool {
        if self.session.is_some() {
            return true;
        }
        let why = self.next_open.clone();
        match self.deps.seat.open(why).await {
            Ok(opened) => {
                tracing::info!(agent = %opened.agent, who = %self.who, "session opened");
                self.agent = Some(opened.agent);
                self.session = Some(Live {
                    agent: opened.agent,
                    handle: opened.handle,
                    preamble: opened.preamble,
                    model: None,
                });
                self.next_open = OpenReason::Startup;
                self.open.blocked = false;
                self.open.shown = false;
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, who = %self.who, "couldn't open the session");
                self.open.blocked = true;
                if !self.open.shown {
                    self.show_open_error(&e).await;
                }
                false
            }
        }
    }

    /// Shows why the session couldn't open, once, as a line from the app.
    async fn show_open_error(&mut self, error: &SeatError) {
        let agent = match self.agent {
            Some(a) => Some(a),
            None => self.own_agent().await.ok().flatten(),
        };
        let Some(agent) = agent else {
            return;
        };
        self.agent = Some(agent);
        let text = error.to_string();
        match self.deps.chat.app_note(agent, None, &text).await {
            Ok(_) => {
                self.open.shown = true;
                self.deps.chat.changed();
            }
            Err(e) => tracing::warn!(error = %e, "couldn't show why the session didn't open"),
        }
    }

    /// The session is gone; the next `open` says why.
    fn lose_session(&mut self, why: OpenReason) {
        self.session = None;
        self.next_open = why;
    }

    /// Closes the session (`Close`, then up to [`INTERRUPT_GRACE`] for `SessionClosed`).
    async fn close_session(&mut self) {
        let Some(mut live) = self.session.take() else {
            return;
        };
        if live.handle.close().is_err() {
            return;
        }
        let clock = Arc::clone(&self.deps.clock);
        let deadline = clock.instant() + INTERRUPT_GRACE;
        loop {
            let sleep = clock.sleep_until(deadline);
            let event = tokio::select! {
                biased;
                ev = live.handle.next_event() => ev,
                () = sleep => {
                    tracing::warn!("the seat's session didn't close within the grace period");
                    return;
                }
            };
            match event {
                None | Some(EngineEvent::SessionClosed { .. }) => return,
                Some(other) => tracing::debug!(event = other.name(), "seat event while closing"),
            }
        }
    }
}

/// The next event of a session, or never when there is none.
async fn next_event(live: Option<&mut Live>) -> Option<EngineEvent> {
    match live {
        Some(live) => live.handle.next_event().await,
        None => std::future::pending().await,
    }
}

/// A close reason in plain words.
fn close_reason(reason: &CloseReason) -> String {
    match reason {
        CloseReason::Requested => "it was closed".to_owned(),
        CloseReason::EndConversation => "EndConversation".to_owned(),
        CloseReason::ResumeFailed { message } => format!("it couldn't be resumed ({message})"),
        CloseReason::Crashed {
            exit_code, signal, ..
        } => match (exit_code, signal) {
            (Some(code), _) => format!("the engine crashed (exit code {code})"),
            (None, Some(sig)) => format!("the engine crashed (signal {sig})"),
            (None, None) => "the engine crashed".to_owned(),
        },
    }
}

/// A local `HH:MM` for an instant.
fn local_hhmm(at: jiff::Timestamp) -> String {
    at.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M")
        .to_string()
}
