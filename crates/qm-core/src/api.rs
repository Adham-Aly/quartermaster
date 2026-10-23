//! The core's end of the link to the terminal UI.
//!
//! Two pieces:
//!
//! - [`Conversation`]: the chat as the `messages` table and the notice events hold it. It
//!   implements the seam's [`ChatLog`] for the turn loop (the app's acknowledgement, notices, app
//!   notes), stores your messages, records your answers on notices, and signals when the chat
//!   changed so the UI gets a fresh snapshot.
//! - [`serve`]: answers the UI's [`Command`]s and sends an [`Update::Snapshot`] after every change
//!   (coalesced: one snapshot per wake-up of the loop), until the UI quits or goes away.
//!
//! **Commands owned elsewhere** slot in as [`ApiPart`]s: each gets the commands the core doesn't
//! handle itself and adds its facts to every snapshot (the settings screens' commands and
//! `Snapshot.settings` are one such part, added in the binary's wiring).

use crate::attachments::{Attachments, Staged};
use crate::turn_io::{ChatLog, Notice, SeatError, SeatInbox, SeatInput, SeatStatus, event_kind};
use qm_policy::markers::neutralise;
use qm_prompts::context_block::cut;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent, NoticeCause, NoticeChoice, NoticeState};
use qm_store::messages::{self, MessageRow, NewMessage, TurnOrigin};
use qm_types::BoxFuture;
use qm_types::api::{
    AgentRef, Card, CardAction, CardButton, CardKind, CardLine, ChatLine, Command, CoreServer,
    CrewRow, NoticeAnswer, SlashCommand, Snapshot, Styled, Tone, Update,
};
use qm_types::clock::Clock;
use qm_types::domain::{MessageKind, MessageSource};
use qm_types::ids::{AgentId, MessageId, TurnId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use tokio::sync::Notify;

/// The most lines of the conversation a snapshot carries.
pub const CONVERSATION_LINES: usize = 200;
/// Your message, as the notice quotes it.
pub const NOTICE_QUOTE_CHARS: usize = 80;
/// Your message, as the `<you>` line for your answer on its notice quotes it.
pub const ANSWER_QUOTE_CHARS: usize = 60;

/// The notice for a turn of yours that did nothing, even after the retry.
pub const NO_ACTION_HEADING: &str = "Nothing came of your message";
pub const NO_ACTION_NOTE: &str = "· after one retry";
/// The notice for a turn of yours that failed.
pub const FAILED_HEADING: &str = "Your message couldn't be finished";
/// A question card's heading.
pub const QUESTION_HEADING: &str = "Pick an answer, or type your own";
/// The Quartermaster's crew row while a turn runs, and otherwise.
pub const WORKING: &str = "working";
pub const READY: &str = "ready";
/// Setup instruction when no model level is configured.
pub const MODEL_SETUP_NOTICE: &str =
    "Configure at least one model level in Settings to start chatting.";

async fn models_configured(conversation: &Conversation) -> bool {
    conversation
        .db
        .call(|conn| Ok(!qm_store::models::list_placements(conn)?.is_empty()))
        .await
        .unwrap_or(false)
}

fn needs_models(command: &Command) -> bool {
    matches!(
        command,
        Command::Send {
            to: AgentRef::Quartermaster,
            ..
        } | Command::AnswerNotice {
            answer: NoticeAnswer::AskAgain,
            ..
        } | Command::RunSlashCommand { .. }
    )
}

/// The chat: what the loop and the tool host write, and what the snapshot shows.
pub struct Conversation {
    db: Db,
    clock: Arc<dyn Clock>,
    agent: AgentId,
    dirty: Arc<Notify>,
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl Conversation {
    /// The Quartermaster's conversation; `agent` is its row (`qm_core::seat::Seat::agent`).
    pub fn new(db: Db, clock: Arc<dyn Clock>, agent: AgentId) -> Self {
        Self {
            db,
            clock,
            agent,
            dirty: Arc::new(Notify::new()),
        }
    }

    /// The Quartermaster's agent row.
    pub fn agent(&self) -> AgentId {
        self.agent
    }

    /// Waits until something the conversation shows changed.
    pub async fn changes(&self) {
        self.dirty.notified().await;
    }

    /// Stores your message to the Quartermaster, neutralised so a marker can only come from the
    /// app, with the paths of the images you pasted (`staged`). `None` for a blank message, which
    /// is ignored.
    pub async fn store_your_message(
        &self,
        text: &str,
        mut staged: Staged,
    ) -> Result<Option<MessageId>, SeatError> {
        if text.trim().is_empty() {
            return Ok(None);
        }
        let text = neutralise(text);
        let agent = self.agent;
        let at = self.clock.now();
        let id = self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let id = messages::insert(
                    &tx,
                    &NewMessage {
                        agent,
                        source: MessageSource::You,
                        kind: MessageKind::Message,
                        text: &text,
                        options: &[],
                        replies_to: None,
                        turn: None,
                        at,
                    },
                )?;
                staged.attach(&tx, id, &text)?;
                tx.commit()?;
                Ok(id)
            })
            .await?;
        Ok(Some(id))
    }

    /// Records your answer on the notice under `message`. Returns `false` (and records nothing)
    /// when there is no notice or it was already answered.
    pub async fn answer_notice(
        &self,
        message: MessageId,
        answer: NoticeChoice,
    ) -> Result<bool, SeatError> {
        let agent = self.agent;
        let at = self.clock.now();
        Ok(self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let Some(row) = messages::get(&tx, message)? else {
                    return Ok(false);
                };
                if row.agent != agent || row.source != MessageSource::You {
                    return Ok(false);
                }
                let state = events::notices(&tx, &[message])?.remove(&message);
                if state.is_none_or(|s| s.answer.is_some()) {
                    return Ok(false);
                }
                let label = match answer {
                    NoticeChoice::AskAgain => "Ask again",
                    NoticeChoice::LeaveIt => "Leave it",
                };
                let summary = format!(
                    "you chose {label} on your message “{}”",
                    cut(&row.text, ANSWER_QUOTE_CHARS)
                );
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: event_kind::NOTICE_ANSWERED,
                        agent: Some(agent),
                        task: None,
                        turn: None,
                        summary: &summary,
                        payload: serde_json::json!({
                            "message": message,
                            "answer": answer.as_str(),
                        }),
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await?)
    }

    /// Everything the snapshot's conversation is built from.
    async fn load(&self) -> Result<ConversationData, SeatError> {
        load_conversation(&self.db, self.agent).await
    }

    /// `agent`'s conversation (a secondmate's), refreshing the same UI as this one: a change to it
    /// re-renders the open session view.
    #[must_use]
    pub fn sibling(&self, agent: AgentId) -> Self {
        Self {
            db: self.db.clone(),
            clock: Arc::clone(&self.clock),
            agent,
            dirty: Arc::clone(&self.dirty),
        }
    }

    fn insert_line(&self, new: OwnedMessage) -> BoxFuture<'_, Result<MessageId, SeatError>> {
        Box::pin(async move {
            let id = self
                .db
                .call(move |conn| {
                    messages::insert(
                        conn,
                        &NewMessage {
                            agent: new.agent,
                            source: new.source,
                            kind: new.kind,
                            text: &new.text,
                            options: &[],
                            replies_to: new.replies_to,
                            turn: new.turn,
                            at: new.at,
                        },
                    )
                })
                .await?;
            self.changed();
            Ok(id)
        })
    }
}

/// A line the app writes, owned so it can cross to the database thread.
struct OwnedMessage {
    agent: AgentId,
    source: MessageSource,
    kind: MessageKind,
    text: String,
    replies_to: Option<MessageId>,
    turn: Option<TurnId>,
    at: jiff::Timestamp,
}

impl ChatLog for Conversation {
    fn message_text(&self, message: MessageId) -> BoxFuture<'_, Result<String, SeatError>> {
        Box::pin(async move {
            self.db
                .call(move |conn| messages::text(conn, message))
                .await?
                .ok_or_else(|| SeatError::Unavailable(format!("message {message} doesn't exist")))
        })
    }

    fn waiting_messages(&self, agent: AgentId) -> BoxFuture<'_, Result<Vec<MessageId>, SeatError>> {
        Box::pin(async move {
            Ok(self
                .db
                .call(move |conn| messages::waiting(conn, agent))
                .await?)
        })
    }

    fn app_acknowledgement<'a>(
        &'a self,
        agent: AgentId,
        turn: TurnId,
        message: MessageId,
        text: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, SeatError>> {
        self.insert_line(OwnedMessage {
            agent,
            source: MessageSource::App,
            kind: MessageKind::Acknowledgement,
            text: text.to_owned(),
            replies_to: Some(message),
            turn: Some(turn),
            at: self.clock.now(),
        })
    }

    fn notice(
        &self,
        agent: AgentId,
        turn: TurnId,
        message: MessageId,
        notice: Notice,
    ) -> BoxFuture<'_, Result<(), SeatError>> {
        Box::pin(async move {
            let at = self.clock.now();
            let (cause, reason) = match notice {
                Notice::NoAction => (NoticeCause::NoAction, None),
                Notice::Failed { reason } => (NoticeCause::Failed, Some(reason)),
            };
            let summary = match &reason {
                None => format!("{NO_ACTION_HEADING} {NO_ACTION_NOTE}"),
                Some(reason) => format!("{FAILED_HEADING} · {reason}"),
            };
            self.db
                .call(move |conn| {
                    events::insert(
                        conn,
                        &NewEvent {
                            kind: event_kind::NOTICE,
                            agent: Some(agent),
                            task: None,
                            turn: Some(turn),
                            summary: &summary,
                            payload: serde_json::json!({
                                "message": message,
                                "cause": cause.as_str(),
                                "reason": reason,
                            }),
                            at,
                        },
                    )
                })
                .await?;
            self.changed();
            Ok(())
        })
    }

    fn app_note<'a>(
        &'a self,
        agent: AgentId,
        turn: Option<TurnId>,
        text: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, SeatError>> {
        self.insert_line(OwnedMessage {
            agent,
            source: MessageSource::App,
            kind: MessageKind::AppNote,
            text: text.to_owned(),
            replies_to: None,
            turn,
            at: self.clock.now(),
        })
    }

    fn changed(&self) {
        self.dirty.notify_one();
    }
}

/// Everything `agent`'s conversation is built from: the Quartermaster's, or a secondmate's for
/// its session view.
pub(crate) async fn load_conversation(
    db: &Db,
    agent: AgentId,
) -> Result<ConversationData, SeatError> {
    Ok(db
        .call(move |conn| {
            let rows = messages::load_recent(conn, agent, CONVERSATION_LINES)?;
            let yours: Vec<MessageId> = rows
                .iter()
                .filter(|r| r.source == MessageSource::You)
                .map(|r| r.id)
                .collect();
            let notes: Vec<MessageId> = rows
                .iter()
                .filter(|r| r.source == MessageSource::App)
                .map(|r| r.id)
                .collect();
            let mut turns: Vec<TurnId> = rows.iter().filter_map(|r| r.turn).collect();
            turns.sort_unstable();
            turns.dedup();
            Ok(ConversationData {
                acknowledgements: messages::latest_acknowledgements(conn, &yours)?,
                notices: events::notices(conn, &yours)?,
                one_line: events::one_line(conn, &notes)?,
                origins: messages::turn_origins(conn, &turns)?,
                rows,
            })
        })
        .await?)
}

/// What the snapshot's conversation is built from.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConversationData {
    /// The newest lines, oldest first, acknowledgements left out.
    pub rows: Vec<MessageRow>,
    /// The newest acknowledgement beside each of your messages.
    pub acknowledgements: BTreeMap<MessageId, String>,
    pub notices: BTreeMap<MessageId, NoticeState>,
    pub origins: BTreeMap<TurnId, TurnOrigin>,
    /// The app's notes drawn on one line (a plan reply sent from its page).
    pub one_line: BTreeSet<MessageId>,
}

/// The snapshot the Bridge shows, from the loaded conversation and the loop's state.
pub(crate) fn snapshot(data: &ConversationData, status: &SeatStatus, machine: &str) -> Snapshot {
    let mut snapshot = Snapshot::default();
    machine.clone_into(&mut snapshot.machine);
    snapshot.turn_running = status.turn_running;
    snapshot.thinking = thinking(data, status);
    let mut root = CrewRow::new(
        AgentRef::Quartermaster,
        0,
        "Quartermaster",
        if status.turn_running { WORKING } else { READY },
    );
    root.busy = status.turn_running;
    snapshot.crew = vec![root];
    snapshot.conversation = chat_lines(data, status);
    snapshot
}

/// Whether the agent is in a turn and hasn't written anything in it yet (a reply, a question or
/// a note; an acknowledgement beside your message doesn't count), so its conversation shows the
/// busy indicator.
pub(crate) fn thinking(data: &ConversationData, status: &SeatStatus) -> bool {
    let Some(turn) = status.running_turn.filter(|_| status.turn_running) else {
        return false;
    };
    !data
        .rows
        .iter()
        .any(|row| row.source == MessageSource::Agent && row.turn == Some(turn))
}

/// The conversation's lines, as the Bridge's chat draws them.
pub(crate) fn chat_lines(data: &ConversationData, status: &SeatStatus) -> Vec<ChatLine> {
    data.rows
        .iter()
        .enumerate()
        .map(|(i, row)| chat_line(data, status, row, &data.rows[i + 1..]))
        .collect()
}

fn chat_line(
    data: &ConversationData,
    status: &SeatStatus,
    row: &MessageRow,
    later: &[MessageRow],
) -> ChatLine {
    let mut line = ChatLine::new(row.id, row.source, row.kind, row.at, row.text.clone());
    match row.source {
        MessageSource::You => {
            line.acknowledgement = data.acknowledgements.get(&row.id).cloned();
            line.queued = status.queued.contains(&row.id);
            line.card = data.notices.get(&row.id).map(|n| notice_card(n, &row.text));
        }
        MessageSource::Agent => {
            line.context = row
                .turn
                .and_then(|t| data.origins.get(&t))
                .map(|origin| context_words(origin, row.kind));
            if row.kind == MessageKind::Question && !row.options.is_empty() {
                // Only one of the card's own options answers it (pressed, or typed word for
                // word); any other message you write meanwhile leaves the card open.
                let answer = later
                    .iter()
                    .filter(|m| m.source == MessageSource::You && m.kind == MessageKind::Message)
                    .find_map(|m| {
                        row.options
                            .iter()
                            .find(|option| option.trim() == m.text.trim())
                    });
                line.card = Some(question_card(&row.options, answer.map(String::as_str)));
            }
        }
        MessageSource::App if data.one_line.contains(&row.id) => line = line.with_one_line(),
        MessageSource::App | MessageSource::Supervisor => {}
    }
    line
}

/// The dim words after the time: the kind on a turn you started, else what woke the
/// Quartermaster.
fn context_words(origin: &TurnOrigin, kind: MessageKind) -> String {
    if origin.by_you {
        kind.to_string()
    } else {
        match &origin.short {
            Some(short) => format!("after {short}"),
            None => "after an app event".to_owned(),
        }
    }
}

/// The notice under your message, with Ask again and Leave it.
pub(crate) fn notice_card(notice: &NoticeState, your_text: &str) -> Card {
    let (heading, note) = match notice.cause {
        NoticeCause::NoAction => (NO_ACTION_HEADING, NO_ACTION_NOTE.to_owned()),
        NoticeCause::Failed => (
            FAILED_HEADING,
            format!(
                "· {}",
                notice.reason.as_deref().unwrap_or("the turn failed")
            ),
        ),
    };
    let mut card = Card::new(CardKind::Notice, heading);
    card.note = Some(note);
    card.lines = vec![CardLine::new(
        2,
        vec![Styled::new(
            format!("“{}”", cut(your_text, NOTICE_QUOTE_CHARS)),
            Tone::Text,
        )],
    )];
    card.buttons = vec![
        CardButton::new("Ask again", 'a', CardAction::AskAgain),
        CardButton::new("Leave it", 'l', CardAction::LeaveIt),
    ];
    card.answered = notice.answer.map(|a| {
        match a {
            NoticeChoice::AskAgain => "Asked again",
            NoticeChoice::LeaveIt => "Left",
        }
        .to_owned()
    });
    card
}

/// The question card: one button per option, keys 1–9; answered once you sent one of its options,
/// which the card then shows.
pub(crate) fn question_card(options: &[String], answer: Option<&str>) -> Card {
    let mut card = Card::new(CardKind::Question, QUESTION_HEADING);
    card.buttons = options
        .iter()
        .zip('1'..='9')
        .map(|(option, key)| {
            CardButton::new(
                format!("{key} · {option}"),
                key,
                CardAction::Reply {
                    text: option.clone(),
                },
            )
        })
        .collect();
    card.answered = answer.map(|option| format!("Answered · {option}"));
    card
}

/// What handling a command led to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handled {
    /// Nothing the UI shows changed.
    Unchanged,
    /// Send a fresh snapshot.
    Changed,
    /// The app is quitting: send `Closing` and stop serving.
    Quit,
    /// Agents are working: ask before quitting.
    ConfirmQuit { working: u32 },
}

/// A part of the core that owns some commands and some of the snapshot (the settings screens,
/// the cards). [`serve`] offers each part the commands it doesn't handle itself, in
/// order, and lets every part add its facts to each snapshot.
pub trait ApiPart: Send + Sync + fmt::Debug {
    /// Handles `command` if it is this part's; `None` if it isn't.
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>>;

    /// Adds this part's facts to a snapshot.
    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()>;
}

/// What [`serve`] needs.
#[derive(Debug)]
pub struct ApiDeps {
    /// The seat's inbox: your messages, `Esc`, Ask again and the quit reach the loop through it.
    pub inbox: SeatInbox,
    pub conversation: Arc<Conversation>,
    /// Where the images pasted into your messages to the Quartermaster are saved.
    pub attachments: Attachments,
    /// This Mac's name, for the top bar.
    pub machine: String,
    pub parts: Vec<Arc<dyn ApiPart>>,
}

/// Serves the UI until it quits or goes away. On quit the seat's loop is told to shut down first,
/// so the running turn is recorded and the session closed.
pub async fn serve(mut server: CoreServer, deps: ApiDeps) {
    let ApiDeps {
        inbox,
        conversation,
        attachments,
        machine,
        parts,
    } = deps;
    // A second handle on the inbox, only for waiting on the loop's state.
    let mut status = inbox.clone();
    let mut loop_alive = true;
    let mut fresh = true;
    loop {
        if fresh {
            fresh = false;
            let mut snap = match conversation.load().await {
                Ok(data) => snapshot(&data, &inbox.status(), &machine),
                Err(e) => {
                    tracing::error!(error = %e, "couldn't read the conversation for the UI");
                    let mut s = Snapshot::default();
                    s.machine.clone_from(&machine);
                    s
                }
            };
            // Before the parts, so the Actions inbox sees it.
            snap.model_setup_notice =
                (!models_configured(&conversation).await).then(|| MODEL_SETUP_NOTICE.to_owned());
            for part in &parts {
                part.fill(&mut snap).await;
            }
            if server.send(Update::Snapshot(Box::new(snap))).is_err() {
                let _ = inbox.send(SeatInput::Shutdown);
                return;
            }
        }
        tokio::select! {
            command = server.recv() => {
                let Some(command) = command else {
                    // The UI went away without quitting.
                    let _ = inbox.send(SeatInput::Shutdown);
                    return;
                };
                match handle(command, &inbox, (&conversation, &attachments), &parts).await {
                    Handled::Unchanged => {}
                    Handled::Changed => fresh = true,
                    Handled::ConfirmQuit { working } => {
                        let _ = server.send(Update::ConfirmQuit { working });
                    }
                    Handled::Quit => {
                        let _ = server.send(Update::Closing);
                        tracing::info!("the UI asked to quit");
                        return;
                    }
                }
            }
            () = conversation.changes() => fresh = true,
            changed = status.status_changed(), if loop_alive => {
                if changed.is_err() {
                    tracing::warn!("the Quartermaster's turn loop has stopped");
                    loop_alive = false;
                }
                fresh = true;
            }
        }
    }
}

/// One command from the UI.
async fn handle(
    command: Command,
    inbox: &SeatInbox,
    (conversation, attachments): (&Conversation, &Attachments),
    parts: &[Arc<dyn ApiPart>],
) -> Handled {
    // Re-read persisted placements, not the UI snapshot: every seat submission path,
    // including card replies, retries and slash shortcuts, must obey setup.
    if needs_models(&command) && !models_configured(conversation).await {
        return Handled::Changed;
    }
    // The quit reaches the parts first: crewmates stop before the seat's `Shutdown`.
    if matches!(command, Command::RequestQuit | Command::Quit) {
        for part in parts {
            if let Some(handled) = part.handle(&command)
                && let confirm @ Handled::ConfirmQuit { .. } = handled.await
            {
                return confirm;
            }
        }
    }
    match command {
        Command::Refresh => Handled::Changed,
        Command::Send {
            to: AgentRef::Quartermaster,
            text,
            images,
        } => match conversation
            .store_your_message(&text, attachments.stage(&text, &images).await)
            .await
        {
            Ok(Some(message)) => {
                send(inbox, SeatInput::User { message });
                Handled::Changed
            }
            Ok(None) => Handled::Unchanged,
            Err(e) => {
                tracing::error!(error = %e, "couldn't store your message");
                Handled::Unchanged
            }
        },
        command @ (Command::Send {
            to: AgentRef::Secondmate { .. },
            ..
        }
        | Command::Interrupt {
            agent: AgentRef::Secondmate { .. },
        }) => to_parts(command, parts).await,
        Command::Interrupt {
            agent: AgentRef::Quartermaster,
        } => {
            send(inbox, SeatInput::Interrupt);
            Handled::Unchanged
        }
        Command::AnswerNotice { message, answer } => {
            let choice = match answer {
                NoticeAnswer::AskAgain => NoticeChoice::AskAgain,
                NoticeAnswer::LeaveIt => NoticeChoice::LeaveIt,
                other => {
                    tracing::warn!(?other, "an answer on a notice this build doesn't know");
                    return Handled::Unchanged;
                }
            };
            match conversation.answer_notice(message, choice).await {
                Ok(true) => {
                    if choice == NoticeChoice::AskAgain {
                        send(inbox, SeatInput::User { message });
                    }
                    Handled::Changed
                }
                Ok(false) => {
                    tracing::debug!(%message, "no unanswered notice under this message");
                    Handled::Unchanged
                }
                Err(e) => {
                    tracing::error!(error = %e, "couldn't record your answer on the notice");
                    Handled::Unchanged
                }
            }
        }
        Command::RunSlashCommand { slash: command } => {
            let status = inbox.status();
            if status.turn_running || !status.queued.is_empty() {
                // It waits its turn; say so, or nothing would show until it runs.
                let text = format!(
                    "{} runs when the Quartermaster's current work ends.",
                    slash_name(command)
                );
                if let Err(e) = conversation
                    .app_note(conversation.agent(), None, &text)
                    .await
                {
                    tracing::warn!(error = %e, "couldn't note a queued slash command");
                }
            }
            send(inbox, SeatInput::Slash(command));
            Handled::Changed
        }
        Command::RequestQuit | Command::Quit => {
            send(inbox, SeatInput::Shutdown);
            Handled::Quit
        }
        other => to_parts(other, parts).await,
    }
}

/// A slash command as you typed it.
fn slash_name(command: SlashCommand) -> &'static str {
    match command {
        SlashCommand::Clear => "/clear",
        SlashCommand::Compact => "/compact",
        _ => "That command",
    }
}

/// Hands a command to the first part that takes it.
async fn to_parts(command: Command, parts: &[Arc<dyn ApiPart>]) -> Handled {
    for part in parts {
        if let Some(handled) = part.handle(&command) {
            return handled.await;
        }
    }
    tracing::debug!(?command, "command not handled in this build");
    Handled::Unchanged
}

fn send(inbox: &SeatInbox, input: SeatInput) {
    if inbox.send(input).is_err() {
        tracing::warn!(?input, "the Quartermaster's turn loop has stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_io::{SeatInboxRx, seat_inbox};
    use qm_types::clock::SystemClock;

    struct Fixture {
        conversation: Conversation,
        attachments: Attachments,
        inbox: SeatInbox,
        rx: SeatInboxRx,
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        async fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "api-gate-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = Db::open(&dir.join("state.db")).unwrap();
            db.call(|conn| {
                conn.execute_batch("INSERT INTO agents (id, role, home, status, created_at, seq) VALUES (1, 'quartermaster', '/h', 'idle', 1, 1);")?;
                qm_store::seq::observe_all(conn)?;
                Ok(())
            }).await.unwrap();
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let (inbox, rx) = seat_inbox();
            Self {
                conversation: Conversation::new(db, Arc::clone(&clock), AgentId::new(1)),
                attachments: Attachments::new(dir.join("images"), clock),
                inbox,
                rx,
                dir,
            }
        }

        async fn handle(&self, command: Command) -> Handled {
            handle(
                command,
                &self.inbox,
                (&self.conversation, &self.attachments),
                &[],
            )
            .await
        }

        async fn placement(&self, present: bool) {
            self.conversation.db.call(move |conn| {
                if present {
                    conn.execute_batch("INSERT INTO placements (id, canonical, tier, created_at, seq) VALUES (1, 'test-model', 'low', 1, 1);")?;
                } else {
                    conn.execute_batch("DELETE FROM placements;")?;
                }
                Ok(())
            }).await.unwrap();
        }

        async fn store_message(&self, text: &str) -> MessageId {
            self.conversation
                .store_your_message(text, self.attachments.stage(text, &[]).await)
                .await
                .unwrap()
                .unwrap()
        }

        async fn assert_allowed(&mut self, message: MessageId) {
            assert_eq!(self.handle(send_command()).await, Handled::Changed);
            assert!(matches!(
                self.rx.inputs.try_recv().unwrap(),
                SeatInput::User { .. }
            ));
            assert_eq!(self.conversation.load().await.unwrap().rows.len(), 2);
            assert_eq!(
                self.handle(Command::AnswerNotice {
                    message,
                    answer: NoticeAnswer::AskAgain
                })
                .await,
                Handled::Changed
            );
            assert!(
                matches!(self.rx.inputs.try_recv().unwrap(), SeatInput::User { message: id } if id == message)
            );
            for slash in [SlashCommand::Clear, SlashCommand::Compact] {
                assert_eq!(
                    self.handle(Command::RunSlashCommand { slash }).await,
                    Handled::Changed
                );
                assert!(
                    matches!(self.rx.inputs.try_recv().unwrap(), SeatInput::Slash(actual) if actual == slash)
                );
            }
        }

        async fn assert_blocked(&mut self, message: MessageId) {
            let before = self.conversation.load().await.unwrap().rows.len();
            for command in [
                send_command(),
                Command::AnswerNotice {
                    message,
                    answer: NoticeAnswer::AskAgain,
                },
                Command::RunSlashCommand {
                    slash: SlashCommand::Clear,
                },
                Command::RunSlashCommand {
                    slash: SlashCommand::Compact,
                },
            ] {
                assert_eq!(self.handle(command).await, Handled::Changed);
                assert!(matches!(
                    self.rx.inputs.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));
                assert_eq!(self.conversation.load().await.unwrap().rows.len(), before);
            }
            let notices = self.conversation.load().await.unwrap().notices;
            assert!(notices.values().all(|notice| notice.answer.is_none()));
        }
    }

    fn send_command() -> Command {
        Command::Send {
            to: AgentRef::Quartermaster,
            text: "hello".into(),
            images: vec![],
        }
    }

    #[tokio::test]
    async fn model_gate_tracks_persisted_placements_and_blocks_all_submission_paths() {
        let mut f = Fixture::new().await;
        // A real unanswered notice ensures Ask again would otherwise mutate state and enqueue.
        let message = f.store_message("retry me").await;
        f.conversation.db.call(move |conn| {
            conn.execute("INSERT INTO turns (id, agent_id, started_by, message_id, started_at, ended_at, status, seq) VALUES (1, 1, 'user', ?1, 1, 2, 'completed', 2)", [message])?;
            Ok(())
        }).await.unwrap();
        f.conversation
            .notice(
                f.conversation.agent(),
                TurnId::new(1),
                message,
                Notice::NoAction,
            )
            .await
            .unwrap();
        assert!(!models_configured(&f.conversation).await);
        // Exercise the busy slash branch too: blocked shortcuts must not store a queued note.
        f.rx.status
            .send(SeatStatus {
                turn_running: true,
                ..SeatStatus::default()
            })
            .unwrap();
        f.assert_blocked(message).await;

        f.placement(true).await;
        assert!(models_configured(&f.conversation).await);
        f.assert_allowed(message).await;
        f.placement(false).await;
        assert!(!models_configured(&f.conversation).await);
        // Use a fresh notice so removal tests retry gating, not an already answered notice.
        let message = f.store_message("retry after removal").await;
        f.conversation
            .notice(
                f.conversation.agent(),
                TurnId::new(1),
                message,
                Notice::NoAction,
            )
            .await
            .unwrap();
        assert_eq!(
            f.handle(Command::AnswerNotice {
                message,
                answer: NoticeAnswer::AskAgain
            })
            .await,
            Handled::Changed
        );
        assert!(
            f.conversation.load().await.unwrap().notices[&message]
                .answer
                .is_none()
        );
        // The original notice is now answered; the blocked commands must still store nothing.
        let before = f.conversation.load().await.unwrap().rows.len();
        for command in [
            send_command(),
            Command::RunSlashCommand {
                slash: SlashCommand::Clear,
            },
            Command::RunSlashCommand {
                slash: SlashCommand::Compact,
            },
        ] {
            assert_eq!(f.handle(command).await, Handled::Changed);
        }
        assert_eq!(f.conversation.load().await.unwrap().rows.len(), before);
        assert!(matches!(
            f.rx.inputs.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}
