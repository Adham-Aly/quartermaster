//! Direct intervention: what happens when you type into a worker's session.
//!
//! Your message goes in unmarked, so the worker treats it as you. In one transaction:
//!
//! 1. your words, neutralised ([`markers::user_text`]); blank is ignored;
//! 2. a `messages` row on the crewmate's agent (`source = you`) and its `you` session line;
//! 3. the `you.intervened` event, so the Quartermaster's next `<you>` shows it;
//! 4. the supervisor's `intervention` wake with your exact words, posted **without a signal**
//!    ([`INTERVENTION_WAKES_SUPERVISOR`]): it rides on the supervisor's next turn,
//!    whatever starts it, so it reconciles without undoing what you said;
//! 5. the task's status: `waiting`, `reported` or `stuck` → `running` through the lifecycle's
//!    `Steered` (a stuck card no longer applies: "You stepped in"); `held` and `settling` stay.
//!
//! After the commit the task service delivers it: slipped into the running turn (on Claude a second
//! user message queued into the turn, on the provider engine the steering queue before the next
//! model call), or a turn of its own. The supervisor sees your words, never the worker's replies:
//! the worker answers you in its own session.
//!
//! A secondmate's arm is [`intervene_secondmate`]: your message goes into the secondmate's own
//! conversation as a turn you started, and steps 3 and 4 tell its supervisor, the Quartermaster
//! ("you told SM-02 directly").

use crate::attachments::Staged;
use crate::held::HeldService;
use crate::inbox::{Inbox, Notify};
use crate::markers;
use crate::secondmates::SecondmateService;
use crate::tasks::TaskService;
use crate::tasks::lifecycle::{TaskEvent, is_exiting, is_open, next};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_store::StoreError;
use qm_store::agents;
use qm_store::events::{self, NewEvent};
use qm_store::held::{HeldPayload, HoldReason};
use qm_store::messages::{self, NewMessage};
use qm_store::sessions::{self, LineKind, NewSessionLine};
use qm_store::tasks as task_store;
use qm_store::wakes::NewWake;
use qm_types::api::AgentRef;
use qm_types::domain::{HeldKind, MessageKind, MessageSource, TaskStatus, WakeKind};
use qm_types::ids::{AgentId, MessageId, SecondmateId, TaskId};
use rusqlite::Connection;
use std::fmt;

/// Your intervention reaches the supervisor on its next turn; it doesn't start one
/// (waking at once would run a Quartermaster turn per message you type to a worker).
pub const INTERVENTION_WAKES_SUPERVISOR: bool = false;

/// Whether a waiting wake of `kind` starts its target's turn by itself, at startup's catch-up too:
/// every kind but an intervention, which rides on the next turn.
pub fn wakes_on_its_own(kind: WakeKind) -> bool {
    kind != WakeKind::Intervention || INTERVENTION_WAKES_SUPERVISOR
}
/// Your words, as the `<you>` line quotes them.
pub const INTERVENED_QUOTE_CHARS: usize = 80;
/// `events.kind` of your direct message.
pub const INTERVENED_EVENT: &str = "you.intervened";
/// `events.kind` of a stuck card your message made moot.
pub const STEPPED_IN_EVENT: &str = "task.stepped_in";

/// Why an intervention didn't go through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intervention {
    /// Nothing but whitespace: ignored.
    Blank,
    /// The task is closed or ending (or has no crewmate yet): shown in the session as an app line.
    Closed { why: String },
    /// Not an agent you write to here (the Quartermaster; a secondmate that isn't running), or
    /// the store failed.
    NotAvailable,
}

impl fmt::Display for Intervention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => f.write_str("the message was blank"),
            Self::Closed { why } => f.write_str(why),
            Self::NotAvailable => f.write_str("messages to this agent aren't available"),
        }
    }
}

/// What direct intervention needs. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ReconcileService {
    tasks: TaskService,
}

impl ReconcileService {
    pub fn new(tasks: TaskService) -> Self {
        Self { tasks }
    }
}

/// What the transaction came to.
enum Recorded {
    Sent { message: MessageId },
    Closed { why: String },
}

/// Your message `text` to `to`, typed into its session.
pub async fn intervene(
    svc: &ReconcileService,
    to: AgentRef,
    text: String,
    mut staged: Staged,
) -> Result<(), Intervention> {
    let words = markers::user_text(&text);
    if words.is_blank() {
        return Err(Intervention::Blank);
    }
    let task = match to {
        AgentRef::Crewmate { task } => task,
        // A secondmate's arm is `intervene_secondmate`, through `SecondmatesPart`.
        AgentRef::Secondmate { .. } | AgentRef::Quartermaster => {
            return Err(Intervention::NotAvailable);
        }
    };
    let tasks = &svc.tasks;
    let at = tasks.clock().now();
    let words = words.into_string();
    let recorded = tasks
        .write(move |tx, notify| record(tx, notify, task, (&words, &mut staged), at))
        .await;
    match recorded {
        Ok(Recorded::Sent { message }) => {
            tasks.deliver_user_message(task, message);
            Ok(())
        }
        Ok(Recorded::Closed { why }) => Err(Intervention::Closed { why }),
        Err(e) => {
            tracing::error!(%task, error = %e, "couldn't record your message to a crewmate");
            Err(Intervention::NotAvailable)
        }
    }
}

/// Your message `text` to secondmate `id`, typed into its session. One transaction: the message on
/// the secondmate's agent (neutralised), the `you.intervened` event and the Quartermaster's
/// unsignalled `intervention` wake. After the commit, delivery: a `User` turn in the
/// secondmate's own session, its reply staying in its own conversation.
pub async fn intervene_secondmate(
    secondmates: &SecondmateService,
    id: SecondmateId,
    text: String,
    mut staged: Staged,
) -> Result<(), Intervention> {
    let words = markers::user_text(&text);
    if words.is_blank() {
        return Err(Intervention::Blank);
    }
    if !secondmates.is_running(id) {
        return Err(Intervention::NotAvailable);
    }
    let at = secondmates.clock().now();
    let words = words.into_string();
    let recorded = secondmates
        .db()
        .call(move |conn| {
            let tx = conn.transaction()?;
            let Some(record) = qm_store::secondmates::load(&tx, id)? else {
                return Ok(None);
            };
            let message = record_secondmate(&tx, &record, (&words, &mut staged), at)?;
            tx.commit()?;
            Ok(Some(message))
        })
        .await;
    match recorded {
        Ok(Some(message)) => {
            secondmates.deliver_user_message(id, message);
            Ok(())
        }
        Ok(None) => Err(Intervention::NotAvailable),
        Err(e) => {
            tracing::error!(%id, error = %e, "couldn't record your message to a secondmate");
            Err(Intervention::NotAvailable)
        }
    }
}

/// A secondmate's message, event and wake, in the caller's transaction.
fn record_secondmate(
    tx: &Connection,
    record: &qm_store::secondmates::SecondmateRecord,
    (words, staged): (&str, &mut Staged),
    at: Timestamp,
) -> Result<MessageId, StoreError> {
    let id = record.id;
    let message = messages::insert(
        tx,
        &NewMessage {
            agent: record.agent,
            source: MessageSource::You,
            kind: MessageKind::Message,
            text: words,
            options: &[],
            replies_to: None,
            turn: None,
            at,
        },
    )?;
    // Your pasted images' paths join your words, here and in every copy below.
    let words = &staged.attach(tx, message, words)?;
    events::insert(
        tx,
        &NewEvent {
            kind: INTERVENED_EVENT,
            agent: Some(record.agent),
            task: None,
            turn: None,
            summary: &format!(
                "you told {id} directly: “{}”",
                quote(words, INTERVENED_QUOTE_CHARS)
            ),
            payload: serde_json::json!({ "message": message }),
            at,
        },
    )?;
    // Unsignalled, so nothing is notified, before or after the commit.
    post_wake(
        tx,
        &mut Notify::new(),
        record.parent,
        (&id.to_string(), None),
        message,
        words,
        at,
    )?;
    Ok(message)
}

/// Steps 2–5, in the caller's transaction.
fn record(
    tx: &Connection,
    notify: &mut Notify,
    task: TaskId,
    (words, staged): (&str, &mut Staged),
    at: Timestamp,
) -> Result<Recorded, StoreError> {
    let row = task_store::load(tx, task)?;
    let crewmate = agents::crewmate_of(tx, task)?;
    let Some(row) = row else {
        return Ok(Recorded::Closed {
            why: format!("{task} doesn't exist, so your message wasn't sent."),
        });
    };
    let Some(crewmate) = crewmate else {
        return Ok(Recorded::Closed {
            why: format!("{task} has no crewmate yet, so your message wasn't sent."),
        });
    };
    if !is_open(row.status) || is_exiting(row.status) {
        let why = format!("{task} is closed, so your message wasn't sent.");
        let mut line = NewSessionLine::new(crewmate.id, LineKind::App, why.clone(), at);
        line.task = Some(task);
        sessions::insert(tx, &line)?;
        return Ok(Recorded::Closed { why });
    }
    let message = messages::insert(
        tx,
        &NewMessage {
            agent: crewmate.id,
            source: MessageSource::You,
            kind: MessageKind::Message,
            text: words,
            options: &[],
            replies_to: None,
            turn: None,
            at,
        },
    )?;
    // Your pasted images' paths join your words, here and in every copy below.
    let words = &staged.attach(tx, message, words)?;
    let mut line = NewSessionLine::new(crewmate.id, LineKind::You, words.as_str(), at);
    line.task = Some(task);
    line.message = Some(message);
    sessions::insert(tx, &line)?;
    events::insert(
        tx,
        &NewEvent {
            kind: INTERVENED_EVENT,
            agent: Some(crewmate.id),
            task: Some(task),
            turn: None,
            summary: &format!(
                "you told {task} directly: “{}”",
                quote(words, INTERVENED_QUOTE_CHARS)
            ),
            payload: serde_json::json!({ "message": message }),
            at,
        },
    )?;
    post_wake(
        tx,
        notify,
        row.supervisor,
        (&task.to_string(), Some(task)),
        message,
        words,
        at,
    )?;
    steer(tx, task, row.status, at)?;
    Ok(Recorded::Sent { message })
}

/// Step 4: the supervisor's `intervention` wake about `who` (a task, or a secondmate with no
/// task), with your exact words.
fn post_wake(
    tx: &Connection,
    notify: &mut Notify,
    supervisor: AgentId,
    (who, task): (&str, Option<TaskId>),
    message: MessageId,
    words: &str,
    at: Timestamp,
) -> Result<(), StoreError> {
    let time = at
        .to_zoned(TimeZone::system())
        .strftime("%H:%M")
        .to_string();
    let text = format!(
        "The user wrote to {who} directly at {time} (you weren't in that conversation, and its \
         replies stay in its session): “{words}”. Update your record of {what} from this and \
         from {who}'s next report.",
        what = if task.is_some() {
            "the task"
        } else {
            "its work"
        },
    );
    let wake = NewWake {
        target: supervisor,
        kind: WakeKind::Intervention,
        payload: serde_json::json!({
            "text": text,
            "short": format!("you wrote to {who}"),
            "message": message,
        }),
        dedupe_key: format!("intervention:{who}:{message}"),
        task,
        at,
    };
    // Written with its fact, but its target is signalled only when the decision says so.
    let mut unsignalled = Notify::new();
    let targets = if wakes_on_its_own(WakeKind::Intervention) {
        notify
    } else {
        &mut unsignalled
    };
    Inbox::post(tx, &wake, targets)?;
    Ok(())
}

/// Step 5: `waiting`, `reported` or `stuck` → `running` where the lifecycle allows `Steered`
/// (only where the lifecycle has that transition; otherwise the status stays); a stuck card is
/// superseded.
fn steer(tx: &Connection, task: TaskId, from: TaskStatus, at: Timestamp) -> Result<(), StoreError> {
    if !matches!(
        from,
        TaskStatus::Waiting | TaskStatus::Reported | TaskStatus::Stuck
    ) {
        return Ok(());
    }
    let Ok(to) = next(from, TaskEvent::Steered) else {
        return Ok(());
    };
    task_store::set_status(tx, task, from, to, at)?;
    if from != TaskStatus::Stuck {
        return Ok(());
    }
    let stuck_cards = HeldService::pending(tx, HeldKind::Decision)?
        .into_iter()
        .filter(|held| {
            held.task == Some(task)
                && matches!(&held.payload, HeldPayload::Decision(d) if d.reason == HoldReason::Stuck)
        });
    for held in stuck_cards {
        if HeldService::supersede(tx, held.id, at)? {
            task_store::event(
                tx,
                STEPPED_IN_EVENT,
                task,
                None,
                &format!("You stepped in on {task}; its stuck card no longer applies"),
                serde_json::json!({ "held": held.id }),
                at,
            )?;
        }
    }
    Ok(())
}

/// `s` cut to `max` characters, ending in `…` when cut.
fn quote(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
