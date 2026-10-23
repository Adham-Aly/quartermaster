//! The seam between the seat's turn loop and the services around it.
//!
//! The turn loop ([`crate::turns`]) runs the Quartermaster's turns. It reaches everything else
//! through the traits here: the seat's engine session
//! ([`SeatSession`], in `seat.rs`), the context block ([`ContextSource`], in `context_block.rs`)
//! and the chat ([`ChatLog`], in `api.rs`). Commands reach the loop through a [`SeatInbox`], which
//! the core's API (`api.rs`) and every wake producer hold; the loop answers with a [`SeatStatus`].

use qm_engine::{EngineError, InitReport, SessionHandle};
use qm_store::StoreError;
use qm_types::BoxFuture;
use qm_types::api::SlashCommand;
use qm_types::ids::{AgentId, MessageId, SessionRef, TurnId};
use std::fmt;
use tokio::sync::{mpsc, watch};

/// The `events.kind` values the seat writes. Kinds starting `you.` are your own actions and
/// choices, which the context block's `<you>` section lists.
pub mod event_kind {
    /// A turn started; written before its context block is built, so "since your last turn" means
    /// events after the previous turn's `seat.turn_started`.
    pub const TURN_STARTED: &str = "seat.turn_started";
    /// A turn ended, with its outcome.
    pub const TURN_ENDED: &str = "seat.turn_ended";
    /// The model's plain final text: the hidden close-out.
    pub const CLOSE_OUT: &str = "seat.close_out";
    /// A limit stopped a turn.
    pub const LIMIT_HIT: &str = "seat.limit_hit";
    /// A notice under one of your messages: no action after the retry, or a failed turn.
    pub const NOTICE: &str = "seat.notice";
    /// You answered a notice: Ask again or Leave it.
    pub const NOTICE_ANSWERED: &str = "you.answered_notice";
    /// The seat's session was opened, resumed or replaced.
    pub const SESSION_OPENED: &str = "seat.session_opened";
    /// The model called Claude's `EndConversation`; the seat reopened its session.
    pub const END_CONVERSATION: &str = "seat.end_conversation";
    /// You ran `/clear`: the session was replaced by one with an empty conversation.
    pub const CLEARED: &str = "seat.cleared";
}

/// Why the loop asks for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenReason {
    /// The app started: resume the stored session if its engine still serves the seat's model.
    Startup,
    /// The engine couldn't resume the session (`SessionClosed { ResumeFailed }`). The turn's input
    /// was never delivered; the loop sends it again on the new session.
    ResumeFailed { message: String },
    /// The session closed for another reason (`EndConversation`, a crash before it was ready).
    Closed { reason: String },
    /// The seat's model moved to another engine.
    EngineSwitch,
    /// The seat's effort level changed (your setting, or its model's tier): resume the same
    /// conversation at the new level where the engine and account still serve it.
    EffortChange,
    /// You ran `/clear`: a new session with an empty conversation, and no summary of earlier
    /// turns.
    Cleared,
}

/// A session the loop can drive.
#[derive(Debug)]
pub struct OpenedSession {
    /// The Quartermaster's agent row.
    pub agent: AgentId,
    pub handle: SessionHandle,
    /// Put first in the next turn's input: the short summary of the last few turns a new session
    /// opens with, as an app-marked block. `None` when the session was resumed.
    pub preamble: Option<String>,
}

/// What a quiet moment between turns led to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleOutcome {
    /// Nothing to do, or the model changed within the same engine (`SetModel` was sent).
    Unchanged,
    /// Close this session and open another.
    Reopen(OpenReason),
}

/// The seat's engine session.
pub trait SeatSession: Send + Sync + fmt::Debug {
    /// Opens the seat's session: resumes the stored one where it can, or starts a new one with a
    /// summary of the last few turns.
    fn open(&self, why: OpenReason) -> BoxFuture<'_, Result<OpenedSession, SeatError>>;

    /// The engine reported `SessionReady`: store the session reference on the agent, record the
    /// init report.
    fn session_ready<'a>(
        &'a self,
        agent: AgentId,
        session: &'a SessionRef,
        resumed: bool,
        init: &'a InitReport,
    ) -> BoxFuture<'a, Result<(), SeatError>>;

    /// Called between turns, never during one: applies a pending change of the seat's model
    /// ("at the next idle moment, never mid-turn").
    fn idle<'a>(
        &'a self,
        agent: AgentId,
        handle: &'a SessionHandle,
    ) -> BoxFuture<'a, Result<IdleOutcome, SeatError>>;

    /// Resolves when a setting the seat's model follows may have changed, so the loop runs
    /// [`Self::idle`] at once instead of before the next turn. Never, by default.
    fn model_settings_changed(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

/// The context block.
pub trait ContextSource: Send + Sync + fmt::Debug {
    /// The `<qm:context …>` block for this turn, rebuilt in full.
    fn context_block(
        &self,
        agent: AgentId,
        turn: TurnId,
    ) -> BoxFuture<'_, Result<String, SeatError>>;
}

/// The notice under one of your messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// "Nothing came of your message · after one retry".
    NoAction,
    /// The turn failed: a limit, a provider error, the engine gone. `reason` is one plain
    /// sentence.
    Failed { reason: String },
}

/// The chat: what the loop shows you.
pub trait ChatLog: Send + Sync + fmt::Debug {
    /// Your message's text, as stored (already neutralised).
    fn message_text(&self, message: MessageId) -> BoxFuture<'_, Result<String, SeatError>>;

    /// Your messages to this agent that no turn has taken yet, oldest first.
    fn waiting_messages(&self, agent: AgentId) -> BoxFuture<'_, Result<Vec<MessageId>, SeatError>>;

    /// The app's own acknowledgement beside your message ("Started: T-0048", "Got it").
    fn app_acknowledgement<'a>(
        &'a self,
        agent: AgentId,
        turn: TurnId,
        message: MessageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, SeatError>>;

    /// A notice under your message, with Ask again and Leave it.
    fn notice(
        &self,
        agent: AgentId,
        turn: TurnId,
        message: MessageId,
        notice: Notice,
    ) -> BoxFuture<'_, Result<(), SeatError>>;

    /// A line from the app in the conversation, such as a failed turn that no message of yours
    /// started.
    fn app_note<'a>(
        &'a self,
        agent: AgentId,
        turn: Option<TurnId>,
        text: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, SeatError>>;

    /// Something the conversation shows changed: send the UI a fresh snapshot.
    fn changed(&self);
}

/// Why the seat's services failed.
#[derive(Debug, thiserror::Error)]
pub enum SeatError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Nothing can serve the seat right now, in a plain sentence ("No subscription can run the
    /// Quartermaster's model.").
    #[error("{0}")]
    Unavailable(String),
}

/// What reaches the turn loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatInput {
    /// Your message (already stored), or "Ask again" on a notice: one turn each.
    User { message: MessageId },
    /// A wake for the seat was written; the loop claims it from the inbox.
    Wake,
    /// `Esc`: stop the running turn, keep the session.
    Interrupt,
    /// A slash command you ran (`/clear`, `/compact`): it waits behind a running turn and your
    /// queued messages, in order.
    Slash(SlashCommand),
    /// The app is quitting: interrupt a running turn, record it, close the session, and end the
    /// loop. An in-house session's transcript stays, so the next start resumes the conversation.
    Shutdown,
    /// The secondmate was retired: as `Shutdown`, but the session is forgotten, transcript and all.
    Retire,
}

/// The loop's state, for the Bridge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeatStatus {
    /// A turn is running, so `Esc` interrupts it.
    pub turn_running: bool,
    /// The message whose turn is running, if you started it.
    pub running_message: Option<MessageId>,
    /// The turn that is running, while one is.
    pub running_turn: Option<TurnId>,
    /// Your messages waiting for a turn, oldest first: shown as queued.
    pub queued: Vec<MessageId>,
}

/// The turn loop has stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the Quartermaster's turn loop has stopped")]
pub struct SeatClosed;

/// The senders' end: held by the API and by every wake producer.
#[derive(Clone, Debug)]
pub struct SeatInbox {
    inputs: mpsc::UnboundedSender<SeatInput>,
    status: watch::Receiver<SeatStatus>,
}

/// The loop's end.
#[derive(Debug)]
pub struct SeatInboxRx {
    pub inputs: mpsc::UnboundedReceiver<SeatInput>,
    pub status: watch::Sender<SeatStatus>,
}

/// A connected inbox.
pub fn seat_inbox() -> (SeatInbox, SeatInboxRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (status_tx, status_rx) = watch::channel(SeatStatus::default());
    (
        SeatInbox {
            inputs: tx,
            status: status_rx,
        },
        SeatInboxRx {
            inputs: rx,
            status: status_tx,
        },
    )
}

impl SeatInbox {
    pub fn send(&self, input: SeatInput) -> Result<(), SeatClosed> {
        self.inputs.send(input).map_err(|_| SeatClosed)
    }

    /// The loop's latest state.
    pub fn status(&self) -> SeatStatus {
        self.status.borrow().clone()
    }

    /// Waits until the loop's state changes.
    pub async fn status_changed(&mut self) -> Result<(), SeatClosed> {
        self.status.changed().await.map_err(|_| SeatClosed)
    }
}
