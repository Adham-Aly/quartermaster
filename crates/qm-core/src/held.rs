//! Held actions: everything that waits for your yes.
//!
//! A tool call stores the **exact payload** with [`HeldService::insert`], inside the caller's
//! transaction, and links the card to a line of the conversation with
//! [`HeldService::link_message`]. When you answer, [`HeldService::answer`]:
//!
//! 1. checks the card is still pending and that your answer is one the card offers (approve or
//!    reject for an approval; for a decision, a choice the card stored, checked with
//!    `qm_policy::holds::check_choice`);
//! 2. runs the kind's [`HeldPerformer`] with the stored payload, exactly as it was written, so
//!    the agent never gets a second chance to change it (a rejection runs nothing);
//! 3. marks the row answered, records your choice as a `you.` event and posts a `held_answered`
//!    wake to whoever asked (dedupe key `held:<id>`) with the performer's note, all in one
//!    transaction, and signals the target only after it commits.
//!
//! A kind with no performer yet answers "That choice isn't available in this build yet" and
//! leaves the card pending; so does a performer that fails, with its reason shown on the card.
//!
//! On top of that:
//!
//! - **The claim.** Before any performer runs, the row moves out of `pending` with a
//!   compare-and-swap, so a double click, two TUIs or a replayed command can't carry out the same
//!   card twice. A performer that fails gives the claim back (the card is pending again).
//! - **The outcome's `you` line and supersession.** A performer may name the `<you>` line of the
//!   answer ([`YouLine`]: "you approved the merge of T-0038 at 77d1e0a"), which is then the
//!   answer's one `you.` event, and may say the card no longer applies (`superseded`). A rejection
//!   runs the kind's [`HeldPerformer::on_reject`] (a settle hold puts its task back to reported).
//! - **Card builders per kind** ([`HeldService::register_card`]), given the facts the hold's
//!   `settle.held` or `task.held` event recorded.
//! - **The conversation anchor** ([`HeldService::anchor`]): a card sits under the newest line of
//!   the Quartermaster's conversation, or under an app note "T-0042 needs you" when that line is
//!   taken; a pending card's line stays in the snapshot even when it's older than the last lines
//!   shown.
//! - **Supersession with a reason** ([`HeldService::supersede_with`]).
//!
//! [`HeldPart`] is the API's end: it answers `Command::AnswerHeld`, draws each held action linked
//! to a conversation line as a card under that line. The status bar's counts come from
//! `crew_view::CrewPart`.

use crate::api::{ApiPart, Conversation, Handled};
use crate::turn_io::ChatLog;
use crate::turn_io::{SeatInbox, SeatInput};
use qm_policy::holds::check_choice;
use qm_policy::markers::neutralise;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::held::{HeldPayload, HeldRow, NewHeld, insert_held, load_held};
use qm_store::messages::{self, MessageRow};
use qm_store::seq;
use qm_store::time::to_millis;
use qm_store::wakes::{NewWake, insert_wake};
use qm_types::BoxFuture;
use qm_types::api::{
    Card, CardAction, CardButton, CardKind, CardLine, ChatLine, Command, HeldAnswer, Snapshot,
    Styled, Tone,
};
use qm_types::clock::Clock;
use qm_types::domain::{
    HeldKind, HeldStatus, MessageKind, MessageSource, ReleaseLaunchStatus, WakeKind,
};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, HeldActionId, MessageId};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

/// A held action as the performers see it: the stored row, payload exactly as written.
pub type LoadedHeld = HeldRow;

/// The event that ties a held action's card to a line of the conversation.
pub const HELD_SHOWN: &str = "held.shown";
/// A pending held action stopped applying, with why (`{"held", "why"}`).
pub const HELD_SUPERSEDED: &str = "held.superseded";
/// The events whose payload (`{"held": id, …}`) carries a card's facts.
pub const CARD_FACT_EVENTS: &[&str] = &["settle.held", "task.held"];
/// Your answer on a card (a `you.` event, so the context block's `<you>` section lists it).
pub const HELD_ANSWERED: &str = "you.answered_held";
/// You changed a pending card's payload before answering it (a project proposal's location).
pub const HELD_CHANGED: &str = "you.changed_held";

/// What a kind without a performer answers.
pub const NOT_AVAILABLE: &str = "That choice isn't available in this build yet";

/// How many links between cards and conversation lines a snapshot looks through.
const LINKS_READ: usize = 1_000;

/// Carries out one kind of held action, exactly as stored.
pub trait HeldPerformer: Send + Sync + fmt::Debug {
    /// Performs `held`'s stored payload for `answer` (an approval, or a decision's choice).
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>>;

    /// What the card shows besides its heading, title and buttons.
    fn card(&self, held: &LoadedHeld) -> HeldCard {
        let _ = held;
        HeldCard::default()
    }

    /// Your Reject on an approval card: what the kind does about it (a settle hold puts its task
    /// back to reported). `None`: nothing was done.
    fn on_reject<'a>(
        &'a self,
        held: &'a LoadedHeld,
    ) -> BoxFuture<'a, Result<Option<HeldOutcome>, HeldError>> {
        let _ = held;
        Box::pin(async { Ok(None) })
    }
}

/// Builds a kind's whole card from its row and the facts its hold recorded (the payload of its
/// `settle.held` or `task.held` event, `Null` when there is none). The service sets the held ID,
/// the answered state and any failure line on top.
pub trait HeldCardBuilder: Send + Sync + fmt::Debug {
    fn card(&self, row: &HeldRow, facts: &serde_json::Value) -> Card;
}

/// What the app did on your answer, one plain sentence for the `held_answered` wake ("Merged PR
/// #212 at a1b2c3d").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldOutcome {
    pub note: String,
    /// The answer's `<you>` line; `None` writes the generic `you.answered_held` event.
    pub you: Option<YouLine>,
    /// The card no longer applies (the branch moved): it's marked superseded, not answered.
    pub superseded: bool,
}

impl HeldOutcome {
    /// An outcome with just its note.
    pub fn new(note: impl Into<String>) -> Self {
        Self {
            note: note.into(),
            you: None,
            superseded: false,
        }
    }
}

/// A `<you>` line: the event's kind and summary (`you.approved_merge`, "you approved the merge of
/// T-0038 at 77d1e0a").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YouLine {
    pub kind: &'static str,
    pub summary: String,
}

/// A card's face, written by its kind's performer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldCard {
    /// Dim, after the title: "· link".
    pub note: Option<String>,
    pub lines: Vec<CardLine>,
    /// Buttons of the kind's own between Approve and Reject ("Change location").
    pub buttons: Vec<CardButton>,
}

/// Why answering a held action didn't go through.
#[derive(Debug, thiserror::Error)]
pub enum HeldError {
    /// A rule said no: the card was already answered, or the answer isn't one it offers.
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// No performer for this kind in this build; the card stays pending.
    #[error("That choice isn't available in this build yet")]
    NotAvailable,
    /// The performer couldn't carry it out, in one plain sentence; the card stays pending.
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Tells an agent's loop that a wake for it was written (after the commit). A signal carries no
/// data: the `wakes` table is the truth, and a lost signal is caught up when the loop next claims
/// its wakes.
pub trait WakeSignal: Send + Sync + fmt::Debug {
    fn signal(&self, agent: AgentId);
}

/// The Quartermaster's loop as a [`WakeSignal`]: every signal becomes `SeatInput::Wake`, and the
/// loop claims whatever is pending for it. (The `Inbox` routes signals per agent.)
#[derive(Clone, Debug)]
pub struct SeatSignal(pub SeatInbox);

impl WakeSignal for SeatSignal {
    fn signal(&self, agent: AgentId) {
        if self.0.send(SeatInput::Wake).is_err() {
            tracing::debug!(%agent, "the Quartermaster's loop has stopped; the wake waits");
        }
    }
}

/// Every agent's loop as a [`WakeSignal`], through the wake inbox: a `held_answered` wake for a
/// crewmate (its GitHub write) reaches its runner, one for the Quartermaster its seat.
#[derive(Clone, Debug)]
pub struct InboxSignal(pub crate::inbox::Inbox);

impl WakeSignal for InboxSignal {
    fn signal(&self, agent: AgentId) {
        let mut notify = crate::inbox::Notify::new();
        notify.add(agent);
        self.0.notify(&notify);
    }
}

/// The held-action machinery.
#[derive(Clone)]
pub struct HeldService {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    clock: Arc<dyn Clock>,
    signal: Arc<dyn WakeSignal>,
    performers: RwLock<HashMap<HeldKind, Arc<dyn HeldPerformer>>>,
    cards: RwLock<HashMap<HeldKind, Arc<dyn HeldCardBuilder>>>,
    /// Cards being carried out right now: a second answer waits for the first.
    busy: Mutex<HashSet<HeldActionId>>,
    /// Why the last attempt on a card failed, shown on it until it's answered.
    failures: Mutex<HashMap<HeldActionId, String>>,
}

impl fmt::Debug for HeldService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<HeldKind> = self
            .inner
            .performers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        f.debug_struct("HeldService")
            .field("performers", &kinds)
            .finish_non_exhaustive()
    }
}

/// A card's state after an answer, for the wake and the event.
struct Decided {
    status: HeldStatus,
    /// "approved", "rejected", "chose Discard the work on".
    verb: String,
    note: Option<String>,
    you: Option<YouLine>,
}

impl HeldService {
    pub fn new(db: Db, clock: Arc<dyn Clock>, signal: Arc<dyn WakeSignal>) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                clock,
                signal,
                performers: RwLock::new(HashMap::new()),
                cards: RwLock::new(HashMap::new()),
                busy: Mutex::new(HashSet::new()),
                failures: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Registers the performer for `kind`, replacing any earlier one.
    pub fn register(&self, kind: HeldKind, performer: Arc<dyn HeldPerformer>) {
        self.inner
            .performers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind, performer);
    }

    /// Registers the card builder for `kind` (any kind), replacing any earlier one. A kind
    /// without one draws its performer's [`HeldCard`] face.
    pub fn register_card(&self, kind: HeldKind, builder: Arc<dyn HeldCardBuilder>) {
        self.inner
            .cards
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind, builder);
    }

    fn card_builder(&self, kind: HeldKind) -> Option<Arc<dyn HeldCardBuilder>> {
        self.inner
            .cards
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kind)
            .cloned()
    }

    /// Marks a pending held action as no longer applying, with the reason the card shows ("the
    /// branch moved to b2c3d4e"), inside the caller's transaction. Returns whether it was pending.
    pub fn supersede_with(
        conn: &Connection,
        held: HeldActionId,
        task: Option<qm_types::ids::TaskId>,
        why: &str,
        at: jiff::Timestamp,
    ) -> Result<bool, StoreError> {
        if !Self::supersede(conn, held, at)? {
            return Ok(false);
        }
        events::insert(
            conn,
            &NewEvent {
                kind: HELD_SUPERSEDED,
                agent: None,
                task,
                turn: None,
                summary: &format!("No longer applies: {why}"),
                payload: serde_json::json!({ "held": held, "why": why }),
                at,
            },
        )?;
        Ok(true)
    }

    /// Shows `held`'s card in the Quartermaster's conversation (`seat_agent`), inside the caller's
    /// transaction: under its newest line, or under a new app note "{label} needs you" when that
    /// line already carries a card (or is one that can get its own: your message, a question).
    pub fn anchor(
        conn: &Connection,
        held: HeldActionId,
        label: &str,
        seat_agent: AgentId,
        at: jiff::Timestamp,
    ) -> Result<MessageId, StoreError> {
        let newest = conn
            .query_row(
                "SELECT id, source, kind FROM messages
                 WHERE agent_id = ?1 AND kind <> 'acknowledgement'
                 ORDER BY seq DESC LIMIT 1",
                params![seat_agent],
                |r| {
                    Ok((
                        r.get::<_, MessageId>(0)?,
                        r.get::<_, MessageSource>(1)?,
                        r.get::<_, MessageKind>(2)?,
                    ))
                },
            )
            .optional()?;
        let free = match newest {
            Some((id, source, kind))
                if source != MessageSource::You && kind != MessageKind::Question =>
            {
                let taken: bool = conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM events WHERE kind = ?1
                                    AND json_extract(payload, '$.message') = ?2)",
                    params![HELD_SHOWN, id],
                    |r| r.get(0),
                )?;
                (!taken).then_some(id)
            }
            _ => None,
        };
        let message = match free {
            Some(id) => id,
            None => messages::insert(
                conn,
                &messages::NewMessage {
                    agent: seat_agent,
                    source: MessageSource::App,
                    kind: MessageKind::AppNote,
                    text: &format!("{label} needs you"),
                    options: &[],
                    replies_to: None,
                    turn: None,
                    at,
                },
            )?,
        };
        Self::link_message(conn, held, message, at)?;
        Ok(message)
    }

    fn performer(&self, kind: HeldKind) -> Option<Arc<dyn HeldPerformer>> {
        self.inner
            .performers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kind)
            .cloned()
    }

    /// Writes a pending held action with its exact payload, inside the caller's transaction.
    pub fn insert(conn: &Connection, held: &NewHeld) -> Result<HeldActionId, StoreError> {
        insert_held(conn, held)
    }

    /// Shows `held`'s card under the conversation line `message`, inside the caller's
    /// transaction.
    pub fn link_message(
        conn: &Connection,
        held: HeldActionId,
        message: MessageId,
        at: jiff::Timestamp,
    ) -> Result<(), StoreError> {
        events::insert(
            conn,
            &NewEvent {
                kind: HELD_SHOWN,
                agent: None,
                task: None,
                turn: None,
                summary: "",
                payload: serde_json::json!({"held": held, "message": message}),
                at,
            },
        )?;
        Ok(())
    }

    /// The pending held actions of `kind`, oldest first.
    pub fn pending(conn: &Connection, kind: HeldKind) -> Result<Vec<HeldRow>, StoreError> {
        let mut stmt = conn.prepare(
            "SELECT id FROM held_actions WHERE status = 'pending' AND kind = ?1 ORDER BY seq",
        )?;
        let ids = stmt
            .query_map(params![kind], |r| r.get::<_, HeldActionId>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(|id| load_held(conn, id)).collect()
    }

    /// Marks a pending held action as no longer applying (a newer proposal replaced it, or the
    /// thing it would act on moved). Returns whether it was pending.
    pub fn supersede(
        conn: &Connection,
        held: HeldActionId,
        at: jiff::Timestamp,
    ) -> Result<bool, StoreError> {
        let voided = conn.execute(
            "UPDATE held_actions SET status = 'superseded', decided_at = ?2
             WHERE id = ?1 AND status = 'pending'",
            params![held, to_millis(at)],
        )? == 1;
        // A release tag's launch ends with its card, so the tag can be proposed again.
        if voided && let Some(launch) = qm_store::releases::for_held(conn, held)? {
            qm_store::releases::set_outcome(
                conn,
                launch.id,
                &[ReleaseLaunchStatus::Proposed],
                &qm_store::releases::Outcome {
                    status: ReleaseLaunchStatus::Superseded,
                    remote_object: None,
                    words: "its card no longer applies",
                    at,
                },
            )?;
        }
        Ok(voided)
    }

    /// Your change to a pending card's stored payload before you answer it (a project proposal's
    /// location). `before` must still be what is stored, the kind can't change, and a
    /// card being carried out or already answered is refused. Approve then performs exactly the new
    /// payload. A refusal is also shown on the card.
    pub async fn amend(
        &self,
        held: HeldActionId,
        before: HeldPayload,
        after: HeldPayload,
        summary: String,
    ) -> Result<(), HeldError> {
        let Some(_guard) = BusyGuard::take(&self.inner, held) else {
            return Err(busy());
        };
        if before.kind() != after.kind() {
            return Err(Refusal::new("a card's kind can't change").into());
        }
        if before.kind() == HeldKind::ReleaseTag {
            // A release tag's card is bound to the exact tag and commit the crewmate proposed.
            return Err(Refusal::new(
                "a release tag's card can't be changed; reject it and ask for a new proposal",
            )
            .into());
        }
        let to_json = |p: &HeldPayload| {
            serde_json::to_string(p).map_err(|source| StoreError::Json {
                column: "held_actions.payload",
                source,
            })
        };
        let (before, after) = (to_json(&before)?, to_json(&after)?);
        let at = self.inner.clock.now();
        let changed = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let row = load_held(&tx, held)?;
                let stored =
                    serde_json::to_string(&row.payload).map_err(|source| StoreError::Json {
                        column: "held_actions.payload",
                        source,
                    })?;
                if row.status != HeldStatus::Pending || stored != before {
                    return Ok(false);
                }
                tx.execute(
                    "UPDATE held_actions SET payload = ?2 WHERE id = ?1 AND status = 'pending'",
                    params![held, after],
                )?;
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: HELD_CHANGED,
                        agent: row.asked_by,
                        task: row.task,
                        turn: None,
                        summary: &summary,
                        payload: serde_json::json!({ "held": held }),
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await?;
        if !changed {
            return Err(Refusal::new("this card was answered or changed meanwhile").into());
        }
        self.forget_failure(held);
        Ok(())
    }

    /// Shows why a change to a card didn't go through, on the card.
    pub fn show_failure(&self, held: HeldActionId, why: String) {
        self.remember_failure(held, why);
    }

    /// Your answer on a card: performs exactly the stored payload, then records the answer and
    /// wakes whoever asked.
    pub async fn answer(&self, held: HeldActionId, answer: HeldAnswer) -> Result<(), HeldError> {
        let Some(guard) = BusyGuard::take(&self.inner, held) else {
            return Err(busy());
        };
        self.answer_holding(guard, answer).await
    }

    /// [`Self::answer`] in the background: the card shows it's being carried out from now on (a
    /// clone can take a while), and `done` runs when it's finished, whatever the outcome.
    pub fn answer_in_background(
        &self,
        held: HeldActionId,
        answer: HeldAnswer,
        done: Box<dyn FnOnce() + Send>,
    ) {
        let Some(guard) = BusyGuard::take(&self.inner, held) else {
            tracing::info!(%held, "this card is already being carried out");
            return;
        };
        let service = self.clone();
        tokio::spawn(async move {
            match service.answer_holding(guard, answer).await {
                Ok(()) => {}
                Err(HeldError::Store(e)) => {
                    tracing::error!(error = %e, %held, "couldn't record your answer on the card");
                    service.remember_failure(held, "couldn't record your answer".to_owned());
                }
                Err(e) => {
                    tracing::info!(%held, reason = %e, "your answer on the card didn't go through");
                    // Shown on the card while it still waits, so you see why and can retry.
                    service.remember_failure(held, e.to_string());
                }
            }
            done();
        });
    }

    async fn answer_holding(&self, guard: BusyGuard, answer: HeldAnswer) -> Result<(), HeldError> {
        let held = guard.held;
        let row = self
            .inner
            .db
            .call(move |conn| load_held(conn, held))
            .await?;
        if row.status != HeldStatus::Pending {
            return Err(Refusal::new(format!("this card was already {}", row.status)).into());
        }
        let (claim, verb) = match &answer {
            HeldAnswer::Reject if row.choices.is_empty() => {
                (HeldStatus::Rejected, "rejected".to_owned())
            }
            HeldAnswer::Approve if row.choices.is_empty() => {
                (HeldStatus::Approved, "approved".to_owned())
            }
            HeldAnswer::Choose { choice, .. } if !row.choices.is_empty() => {
                check_choice(&row.choices, choice)?;
                (HeldStatus::Answered, format!("chose {} on", choice.label()))
            }
            HeldAnswer::Approve | HeldAnswer::Reject => {
                return Err(
                    Refusal::new("this card is a decision; pick one of its choices").into(),
                );
            }
            HeldAnswer::Choose { .. } => {
                return Err(Refusal::new("this card takes Approve or Reject").into());
            }
        };
        // The row leaves `pending` (a compare-and-swap) before any performer runs, so the same card
        // is never carried out twice.
        if !self.claim(held, claim).await? {
            return Err(Refusal::new("this card was answered meanwhile").into());
        }
        let outcome = match &answer {
            HeldAnswer::Reject => self.reject(&row).await,
            _ => self.perform(&row, &answer).await.map(Some),
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                // The performer didn't carry it out: the card is pending again.
                self.release(held, claim).await?;
                return Err(e);
            }
        };
        let (status, note, you) = match outcome {
            Some(o) if o.superseded => (HeldStatus::Superseded, Some(o.note), o.you),
            Some(o) => (claim, Some(o.note), o.you),
            None => (claim, None, None),
        };
        self.forget_failure(held);
        let decided = Decided {
            status,
            verb,
            note,
            you,
        };
        let target = self.record(&row, &answer, claim, decided).await?;
        if let Some(target) = target {
            self.inner.signal.signal(target);
        }
        Ok(())
    }

    /// Startup: records the answer on a card the last run claimed (`approved` or `rejected`, its
    /// answer not yet recorded) whose performer got far enough that it can't be given back, with
    /// the outcome it came to, and wakes whoever asked.
    pub async fn finish_claimed(
        &self,
        row: &HeldRow,
        outcome: HeldOutcome,
    ) -> Result<(), HeldError> {
        let (answer, verb) = match row.status {
            HeldStatus::Approved => (HeldAnswer::Approve, "approved"),
            HeldStatus::Rejected => (HeldAnswer::Reject, "rejected"),
            other => return Err(Refusal::new(format!("this card was already {other}")).into()),
        };
        let decided = Decided {
            status: if outcome.superseded {
                HeldStatus::Superseded
            } else {
                row.status
            },
            verb: verb.to_owned(),
            note: Some(outcome.note),
            you: outcome.you,
        };
        if let Some(target) = self.record(row, &answer, row.status, decided).await? {
            self.inner.signal.signal(target);
        }
        Ok(())
    }

    /// Moves a pending row to `claim` (its answer not yet recorded). `false` if it was no longer
    /// pending.
    async fn claim(&self, held: HeldActionId, claim: HeldStatus) -> Result<bool, StoreError> {
        let at = self.inner.clock.now();
        self.inner
            .db
            .call(move |conn| {
                Ok(conn.execute(
                    "UPDATE held_actions SET status = ?2, decided_at = ?3
                     WHERE id = ?1 AND status = 'pending'",
                    params![held, claim, to_millis(at)],
                )? == 1)
            })
            .await
    }

    /// Gives a claim back after the performer didn't carry the card out.
    async fn release(&self, held: HeldActionId, claim: HeldStatus) -> Result<(), StoreError> {
        self.inner
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE held_actions SET status = 'pending', decided_at = NULL
                     WHERE id = ?1 AND status = ?2 AND answer IS NULL",
                    params![held, claim],
                )?;
                Ok(())
            })
            .await
    }

    /// Runs the kind's performer on the stored row; a failure is remembered for the card.
    async fn perform(&self, row: &HeldRow, answer: &HeldAnswer) -> Result<HeldOutcome, HeldError> {
        let Some(performer) = self.performer(row.kind) else {
            self.remember_failure(row.id, NOT_AVAILABLE.to_owned());
            return Err(HeldError::NotAvailable);
        };
        match performer.perform(row, answer).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                self.remember_failure(row.id, e.to_string());
                Err(e)
            }
        }
    }

    /// Runs the kind's rejection, if it has a performer; a failure is remembered for the card.
    async fn reject(&self, row: &HeldRow) -> Result<Option<HeldOutcome>, HeldError> {
        let Some(performer) = self.performer(row.kind) else {
            return Ok(None);
        };
        match performer.on_reject(row).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                self.remember_failure(row.id, e.to_string());
                Err(e)
            }
        }
    }

    /// Records the answer on the claimed row (only if it's still this claim, unrecorded), the
    /// `you.` event and the `held_answered` wake in one transaction. Returns the agent to signal.
    async fn record(
        &self,
        row: &HeldRow,
        answer: &HeldAnswer,
        claim: HeldStatus,
        decided: Decided,
    ) -> Result<Option<AgentId>, HeldError> {
        let at = self.inner.clock.now();
        let id = row.id;
        let asked_by = row.asked_by;
        let task = row.task;
        let title = neutralise(&row.title);
        let answer_json = serde_json::to_string(answer).map_err(|source| StoreError::Json {
            column: "held_actions.answer",
            source,
        })?;
        let Decided {
            status,
            verb,
            note,
            you,
        } = decided;
        let (event_kind, summary) = match you {
            Some(you) => (you.kind, you.summary),
            None => (HELD_ANSWERED, format!("you {verb} “{title}”")),
        };
        let text = match &note {
            Some(note) => format!("The user {verb} “{title}”. {note}"),
            None => format!("The user {verb} “{title}”; nothing was done."),
        };
        let short = format!("you {}", verb.trim_end_matches(" on"));
        let answer_value = serde_json::to_value(answer).unwrap_or(serde_json::Value::Null);
        let recorded = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let changed = tx.execute(
                    "UPDATE held_actions SET status = ?2, answer = ?3, decided_at = ?4
                     WHERE id = ?1 AND status = ?5 AND answer IS NULL",
                    params![id, status, answer_json, to_millis(at), claim],
                )?;
                if changed != 1 {
                    return Ok(None);
                }
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: event_kind,
                        agent: asked_by,
                        task,
                        turn: None,
                        summary: &summary,
                        payload: serde_json::json!({
                            "held": id,
                            "answer": answer_value,
                            "note": note,
                        }),
                        at,
                    },
                )?;
                let woke = match asked_by {
                    Some(target) => insert_wake(
                        &tx,
                        &NewWake {
                            target,
                            kind: WakeKind::HeldAnswered,
                            payload: serde_json::json!({
                                "text": text,
                                "short": short,
                                "held": id,
                            }),
                            dedupe_key: format!("held:{id}"),
                            task,
                            at,
                        },
                    )?
                    .map(|_| target),
                    None => None,
                };
                tx.commit()?;
                Ok(Some(woke))
            })
            .await?;
        match recorded {
            Some(target) => Ok(target),
            None => Err(Refusal::new("this card was answered meanwhile").into()),
        }
    }

    fn remember_failure(&self, held: HeldActionId, why: String) {
        self.inner
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(held, why);
    }

    fn forget_failure(&self, held: HeldActionId) {
        self.inner
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&held);
    }

    fn failure(&self, held: HeldActionId) -> Option<String> {
        self.inner
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&held)
            .cloned()
    }

    fn is_busy(&self, held: HeldActionId) -> bool {
        self.inner
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&held)
    }

    /// The card for one held action, from its kind's builder when one is registered.
    fn card(&self, row: &HeldRow, facts: &CardFacts) -> Card {
        let mut card = match self.card_builder(row.kind) {
            Some(builder) => {
                let mut card = builder.card(row, &facts.facts);
                card.held = Some(row.id);
                card
            }
            None => self.face_card(row),
        };
        card.answered = match row.status {
            _ if self.is_busy(row.id) => Some("Working on it…".to_owned()),
            HeldStatus::Pending => {
                if let Some(why) = self.failure(row.id) {
                    card.lines
                        .push(CardLine::new(2, vec![Styled::new(why, Tone::Error)]));
                }
                None
            }
            HeldStatus::Approved => Some("Approved".to_owned()),
            HeldStatus::Rejected => Some("Rejected".to_owned()),
            HeldStatus::Answered => Some(match &row.answer {
                Some(HeldAnswer::Choose { choice, .. }) => format!("Chose {}", choice.label()),
                _ => "Answered".to_owned(),
            }),
            HeldStatus::Superseded => Some(match &facts.superseded {
                Some(why) => format!("No longer applies: {why}"),
                None => "No longer applies".to_owned(),
            }),
        };
        card
    }

    /// Every pending held action, oldest first, with the card the conversation shows for it and
    /// the line that card hangs on: the Actions inbox's held items, whatever their age.
    pub async fn pending_cards(
        &self,
    ) -> Result<Vec<(HeldRow, Card, Option<MessageId>)>, StoreError> {
        let rows = self
            .inner
            .db
            .call(|conn| {
                let mut stmt = conn
                    .prepare("SELECT id FROM held_actions WHERE status = 'pending' ORDER BY seq")?;
                let ids = stmt
                    .query_map([], |r| r.get::<_, HeldActionId>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    let row = load_held(conn, id)?;
                    let facts = card_facts(conn, &row)?;
                    let anchor: Option<MessageId> = conn
                        .query_row(
                            "SELECT m.id FROM events e JOIN messages m
                               ON m.id = json_extract(e.payload, '$.message')
                             WHERE e.kind = ?1 AND json_extract(e.payload, '$.held') = ?2
                             ORDER BY e.seq DESC LIMIT 1",
                            params![HELD_SHOWN, id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    out.push((row, facts, anchor));
                }
                Ok(out)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(row, facts, anchor)| {
                let card = self.card(&row, &facts);
                (row, card, anchor)
            })
            .collect())
    }

    /// The card drawn from the kind's performer's face (kinds without a builder).
    fn face_card(&self, row: &HeldRow) -> Card {
        let face = self
            .performer(row.kind)
            .map(|p| p.card(row))
            .unwrap_or_default();
        let decision = !row.choices.is_empty();
        let mut card = if decision {
            Card::new(CardKind::Decision, "Decision")
        } else {
            Card::new(CardKind::Approval, "Approval needed")
        };
        card.held = Some(row.id);
        card.title = Some(row.title.clone());
        card.note = face.note;
        card.lines = face.lines;
        let extra = face.buttons;
        card.buttons = if decision {
            row.choices
                .iter()
                .zip('1'..='9')
                .map(|(choice, key)| {
                    CardButton::new(
                        format!("{key} · {}", choice.label()),
                        key,
                        CardAction::Answer {
                            answer: HeldAnswer::Choose {
                                choice: choice.clone(),
                                words: None,
                            },
                        },
                    )
                })
                .collect()
        } else {
            vec![CardButton::new(
                "Approve",
                'a',
                CardAction::Answer {
                    answer: HeldAnswer::Approve,
                },
            )]
            .into_iter()
            .chain(extra)
            .chain([CardButton::new(
                "Reject",
                'r',
                CardAction::Answer {
                    answer: HeldAnswer::Reject,
                },
            )])
            .collect()
        };
        card
    }
}

/// The refusal for a second answer while the first is carried out.
fn busy() -> HeldError {
    Refusal::new("this card is already being carried out; wait for it to finish").into()
}

/// Holds a card in the busy set while it's carried out.
struct BusyGuard {
    inner: Arc<Inner>,
    held: HeldActionId,
}

impl BusyGuard {
    fn take(inner: &Arc<Inner>, held: HeldActionId) -> Option<Self> {
        inner
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(held)
            .then(|| Self {
                inner: Arc::clone(inner),
                held,
            })
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.inner
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.held);
    }
}

/// What a card is drawn from besides its row.
#[derive(Clone, Debug, Default)]
struct CardFacts {
    /// The payload of its hold's `settle.held` or `task.held` event (`Null` if none).
    facts: serde_json::Value,
    /// Why it no longer applies, when superseded with a reason.
    superseded: Option<String>,
}

/// What a snapshot needs from the store about held actions.
struct HeldFacts {
    /// The cards to draw, by the conversation line they're under.
    cards: Vec<(MessageId, HeldRow, CardFacts)>,
    /// Lines older than the snapshot's that carry a pending card: they stay shown.
    older: Vec<MessageRow>,
}

/// The newest event of one of `kinds` about `held` (its payload's `held`), for its task.
fn held_event(
    conn: &Connection,
    task: Option<qm_types::ids::TaskId>,
    kinds: &[&str],
    held: HeldActionId,
) -> Result<Option<serde_json::Value>, StoreError> {
    let Some(task) = task else {
        return Ok(None);
    };
    for kind in kinds {
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM events
                 WHERE task_id = ?1 AND kind = ?2 AND json_extract(payload, '$.held') = ?3
                 ORDER BY seq DESC LIMIT 1",
                params![task, kind, held],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(payload) = payload {
            return serde_json::from_str(&payload)
                .map(Some)
                .map_err(|source| StoreError::Json {
                    column: "events.payload",
                    source,
                });
        }
    }
    Ok(None)
}

fn card_facts(conn: &Connection, row: &HeldRow) -> Result<CardFacts, StoreError> {
    let facts = if row.kind == HeldKind::ReleaseTag {
        // A release tag's card shows its launch as it stands now, and once the tag is published,
        // its GitHub release and workflow runs as their own lines.
        qm_store::releases::for_held(conn, row.id)?.map_or(serde_json::Value::Null, |l| {
            let watched = (l.status == ReleaseLaunchStatus::Published && l.observe_state.is_some())
                .then(|| crate::delivery::observe::report(&l));
            serde_json::json!({
                "status": l.status,
                "words": l.remote_words,
                "remote_object": l.remote_object,
                "release": watched.as_ref().and_then(|r| r.release.clone()),
                "workflows": watched.as_ref().and_then(|r| r.workflows.clone()),
                "watch": watched.as_ref().and_then(|r| r.watch.clone()),
            })
        })
    } else {
        held_event(conn, row.task, CARD_FACT_EVENTS, row.id)?.unwrap_or_default()
    };
    let superseded = if row.status == HeldStatus::Superseded {
        held_event(conn, row.task, &[HELD_SUPERSEDED], row.id)?
            .and_then(|p| p.get("why").and_then(|w| w.as_str()).map(str::to_owned))
    } else {
        None
    };
    Ok(CardFacts { facts, superseded })
}

/// Rebuild volatile conversation anchors for durable pending actions. A restart keeps the exact
/// held payload, but neither its `held.shown` event nor its conversation message.
fn reanchor_pending(
    conn: &Connection,
    seat: AgentId,
    at: jiff::Timestamp,
) -> Result<(), StoreError> {
    let missing = {
        let mut stmt = conn.prepare(
            "SELECT h.id, h.title FROM held_actions h
             WHERE h.status = 'pending' AND NOT EXISTS (
                 SELECT 1 FROM events e JOIN messages m
                   ON m.id = json_extract(e.payload, '$.message')
                 WHERE e.kind = ?1 AND json_extract(e.payload, '$.held') = h.id
                   AND m.agent_id = ?2
             ) ORDER BY h.seq",
        )?;
        stmt.query_map(params![HELD_SHOWN, seat], |r| {
            Ok((r.get::<_, HeldActionId>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (held, title) in missing {
        HeldService::anchor(conn, held, &title, seat, at)?;
    }
    Ok(())
}

fn held_facts(conn: &Connection, lines: &HashSet<MessageId>) -> Result<HeldFacts, StoreError> {
    let mut linked: BTreeMap<MessageId, HeldActionId> = BTreeMap::new();
    let mut older_pending: BTreeMap<MessageId, HeldActionId> = BTreeMap::new();
    let mut seen: HashSet<HeldActionId> = HashSet::new();
    for event in events::latest(conn, HELD_SHOWN, LINKS_READ)? {
        let message = event
            .payload
            .get("message")
            .and_then(serde_json::Value::as_i64);
        let held = event
            .payload
            .get("held")
            .and_then(serde_json::Value::as_i64);
        if let (Some(message), Some(held)) = (message, held) {
            let message = MessageId::new(message);
            let held = HeldActionId::new(held);
            let first_link = seen.insert(held);
            if lines.contains(&message) {
                // Newest first: the newest link for a line wins.
                linked.entry(message).or_insert(held);
            } else if first_link {
                older_pending.entry(message).or_insert(held);
            }
        }
    }
    let mut cards = Vec::new();
    for (message, held) in linked {
        // Forgetting can leave historical links behind after deleting their held row.
        let row = match load_held(conn, held) {
            Ok(row) => row,
            Err(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => continue,
            Err(e) => return Err(e),
        };
        let facts = card_facts(conn, &row)?;
        cards.push((message, row, facts));
    }
    // A pending card keeps its line in the snapshot even when the line is older than those shown.
    // Oldest first, by creation: message IDs are random, so the map's order says nothing.
    let mut older = Vec::new();
    for (message, held) in older_pending {
        // Forgetting can leave historical links behind after deleting their held row.
        let row = match load_held(conn, held) {
            Ok(row) => row,
            Err(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => continue,
            Err(e) => return Err(e),
        };
        if row.status != HeldStatus::Pending {
            continue;
        }
        if let Some(line) = messages::get(conn, message)? {
            let facts = card_facts(conn, &row)?;
            cards.push((message, row, facts));
            let seq = seq::of(conn, "messages", message.get(), 0)?;
            older.push((seq, line));
        }
    }
    older.sort_by_key(|(seq, _)| *seq);
    let older = older.into_iter().map(|(_, line)| line).collect();
    Ok(HeldFacts { cards, older })
}

/// The API's end of held actions: `AnswerHeld` and the cards (the status bar's counts are
/// `crew_view::CrewPart`'s).
#[derive(Debug)]
pub struct HeldPart {
    held: HeldService,
    conversation: Arc<Conversation>,
}

impl HeldPart {
    pub fn new(held: HeldService, conversation: Arc<Conversation>) -> Self {
        Self { held, conversation }
    }
}

impl ApiPart for HeldPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        let Command::AnswerHeld { held, answer } = command else {
            return None;
        };
        let conversation = Arc::clone(&self.conversation);
        self.held.answer_in_background(
            *held,
            answer.clone(),
            Box::new(move || conversation.changed()),
        );
        Some(Box::pin(async { Handled::Changed }))
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let lines: HashSet<MessageId> = snapshot.conversation.iter().map(|l| l.id).collect();
            let seat = self.conversation.agent();
            let at = self.held.inner.clock.now();
            let facts = match self
                .held
                .inner
                .db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    reanchor_pending(&tx, seat, at)?;
                    let facts = held_facts(&tx, &lines)?;
                    tx.commit()?;
                    Ok(facts)
                })
                .await
            {
                Ok(facts) => facts,
                Err(e) => {
                    tracing::error!(error = %e, "couldn't read the held actions for the UI");
                    return;
                }
            };
            let cards: HashMap<MessageId, Card> = facts
                .cards
                .iter()
                .map(|(message, row, card_facts)| (*message, self.held.card(row, card_facts)))
                .collect();
            if !facts.older.is_empty() {
                // Older than every line shown, so they go in front, in creation order.
                let shown: HashSet<MessageId> =
                    snapshot.conversation.iter().map(|l| l.id).collect();
                let mut conversation: Vec<ChatLine> = facts
                    .older
                    .into_iter()
                    .filter(|row| !shown.contains(&row.id))
                    .map(|row| ChatLine::new(row.id, row.source, row.kind, row.at, row.text))
                    .collect();
                conversation.append(&mut snapshot.conversation);
                snapshot.conversation = conversation;
            }
            for line in &mut snapshot.conversation {
                if let Some(card) = cards.get(&line.id) {
                    line.card = Some(card.clone());
                }
            }
        })
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[derive(Debug)]
    struct NoSignal;

    impl WakeSignal for NoSignal {
        fn signal(&self, _: AgentId) {}
    }

    #[tokio::test]
    async fn dangling_shown_does_not_block_reconstructing_unrelated_pending_card() {
        // Exercise both the visible-link and older-link paths through held_facts.
        for visible in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "qm-held-dangling-{}-{visible}-{}.db",
                std::process::id(),
                jiff::Timestamp::now().as_nanosecond()
            ));
            let db = Db::open(&path).expect("database");
            let seat = AgentId::new(1);
            let (pending, dangling) = db
                .call(move |conn| {
                    conn.execute_batch(
                        "INSERT INTO agents (id, role, home, status, created_at, seq)
                     VALUES (1, 'quartermaster', '/h', 'idle', 0, 1);",
                    )?;
                    let at = jiff::Timestamp::from_millisecond(1000).expect("time");
                    let new = NewHeld {
                        payload: HeldPayload::Secondmate(qm_store::held::SecondmatePayload {
                            name: "Reviewer".into(),
                            scope: "Review changes".into(),
                            base_branch: None,
                        }),
                        choices: vec![],
                        title: "Create Reviewer".into(),
                        task: None,
                        asked_by: Some(seat),
                        at,
                    };
                    let deleted = HeldService::insert(conn, &new)?;
                    let message = HeldService::anchor(conn, deleted, "Deleted card", seat, at)?;
                    conn.execute("DELETE FROM held_actions WHERE id = ?1", [deleted])?;
                    // This durable card has no message or held.shown history of its own.
                    let pending = HeldService::insert(conn, &new)?;
                    Ok((
                        pending,
                        messages::get(conn, message)?.expect("dangling anchor"),
                    ))
                })
                .await
                .expect("seed dangling link and unrelated pending row");
            let clock: Arc<dyn Clock> = Arc::new(qm_types::clock::SystemClock);
            let conversation = Arc::new(Conversation::new(db.clone(), clock.clone(), seat));
            let service = HeldService::new(db.clone(), clock, Arc::new(NoSignal));
            let part = HeldPart::new(service, conversation);
            let mut snapshot = Snapshot::default();
            if visible {
                snapshot.conversation.push(ChatLine::new(
                    dangling.id,
                    dangling.source,
                    dangling.kind,
                    dangling.at,
                    dangling.text,
                ));
            }
            part.fill(&mut snapshot).await;
            let cards: Vec<_> = snapshot
                .conversation
                .iter()
                .filter_map(|line| line.card.as_ref())
                .collect();
            assert_eq!(cards.len(), 1, "dangling link must not abort fill");
            assert_eq!(cards[0].held, Some(pending));
            assert_eq!(cards[0].answered, None);
            db.call(move |conn| {
                let facts = held_facts(conn, &HashSet::new())?;
                assert_eq!(facts.cards.len(), 1, "reconstructed anchor was committed");
                assert_eq!(facts.cards[0].1.id, pending);
                Ok(())
            })
            .await
            .expect("persisted reconstruction");
            drop(part);
            drop(db);
            std::fs::remove_file(path).expect("remove test database");
        }
    }

    #[test]
    fn pending_row_without_messages_or_events_gets_an_accessible_anchor() {
        let mut conn = Connection::open_in_memory().expect("memory");
        qm_store::schema::migrate(&mut conn).expect("schema");
        conn.execute_batch(
            "INSERT INTO agents (id, role, home, status, created_at, seq)
             VALUES (1, 'quartermaster', '/h', 'idle', 0, 1);",
        )
        .expect("seat");
        let seat = AgentId::new(1);
        let at = jiff::Timestamp::from_millisecond(1000).expect("time");
        let payload = HeldPayload::Secondmate(qm_store::held::SecondmatePayload {
            name: "Reviewer".into(),
            scope: "Review changes".into(),
            base_branch: None,
        });
        let held = HeldService::insert(
            &conn,
            &NewHeld {
                payload: payload.clone(),
                choices: vec![],
                title: "Create Reviewer".into(),
                task: None,
                asked_by: Some(seat),
                at,
            },
        )
        .expect("durable pending row");
        for table in ["events", "messages"] {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "restart has no historical {table}");
        }
        reanchor_pending(&conn, seat, at).expect("reanchor");
        let facts = held_facts(&conn, &HashSet::new()).expect("snapshot facts");
        assert_eq!(facts.cards.len(), 1);
        assert_eq!(
            facts.older.len(),
            1,
            "the first snapshot includes the fresh anchor"
        );
        let (message, row, _) = &facts.cards[0];
        assert_eq!(*message, facts.older[0].id);
        assert_eq!(row.id, held);
        assert_eq!(row.status, HeldStatus::Pending);
        assert_eq!(
            row.payload, payload,
            "approval still uses the exact stored payload"
        );
        assert_eq!(facts.older[0].text, "Create Reviewer needs you");

        reanchor_pending(&conn, seat, at).expect("next snapshot");
        let count: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "snapshot fills do not duplicate anchors");
    }
}
