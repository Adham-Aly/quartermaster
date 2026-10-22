//! Carrying out your choice on a decision card or a stuck notice.
//!
//! [`DecisionPerformer`] is registered with `HeldService` for `HeldKind::Decision`. It performs
//! only a choice the card stored (`HeldService` checks it with `qm_policy::holds::check_choice`,
//! and so does this module), exactly:
//!
//! | Choice | The app does |
//! |---|---|
//! | Retry with my guidance | Your words (required) neutralised and stored on the crewmate as a message from you; the counter behind the hold reset; `held → running`; your words delivered as an unmarked message |
//! | Try a premium / stronger model | A new crewmate at exactly the card's tier, on your card choice's authority, with the failed attempt and your words; the old task stopped |
//! | Discard the work | The task discarded |
//! | Continue | `stuck → running` |
//! | Drop task | The task stopped |
//!
//! The repair counter's once-per-task rerun stays used.

use super::DecisionService;
use super::cards;
use crate::held::{HeldCard, HeldError, HeldOutcome, HeldPerformer, LoadedHeld, YouLine};
use crate::inbox::Notify;
use crate::tasks::lifecycle::{TaskEvent, next_for};
use crate::verification::repair::{load_state, save_state};
use qm_policy::holds::check_choice;
use qm_policy::markers::neutralise;
use qm_store::StoreError;
use qm_store::agents;
use qm_store::held::{HeldPayload, HoldReason};
use qm_store::messages::{self, NewMessage};
use qm_store::tasks as task_store;
use qm_store::verification as store;
use qm_types::BoxFuture;
use qm_types::api::{HeldAnswer, HeldChoice};
use qm_types::domain::{MessageKind, MessageSource, TaskStatus};
use qm_types::error::Refusal;
use qm_types::ids::{MessageId, TaskId};
use rusqlite::Connection;

/// `events.kind` of a retry on a stronger model.
pub const RETRIED_EVENT: &str = "outcome.retried";
/// `events.kind` of a discard you chose.
pub const DISCARDED_EVENT: &str = "outcome.discarded";
/// `events.kind` of your choice on a decision card (the `<you>` line `HeldService` writes).
pub const CHOSE_EVENT: &str = "you.chose";

/// Carries out decision cards and stuck notices.
#[derive(Clone, Debug)]
pub struct DecisionPerformer {
    service: DecisionService,
}

impl DecisionPerformer {
    pub fn new(service: DecisionService) -> Self {
        Self { service }
    }
}

impl HeldPerformer for DecisionPerformer {
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>> {
        Box::pin(async move { self.service.perform(held, answer).await })
    }

    fn card(&self, held: &LoadedHeld) -> HeldCard {
        cards::face(held)
    }
}

/// Resets the counter behind the hold and releases the task, in one transaction with your words
/// stored on the crewmate. Returns the message to deliver.
fn release_with_words(
    conn: &Connection,
    task: TaskId,
    reason: HoldReason,
    words: &str,
    label: &str,
    at: jiff::Timestamp,
) -> Result<Result<MessageId, Refusal>, StoreError> {
    let Some(row) = task_store::load(conn, task)? else {
        return Ok(Err(Refusal::new(format!("{task} doesn't exist"))));
    };
    let Some(crewmate) = agents::crewmate_of(conn, task)? else {
        return Ok(Err(Refusal::new(format!(
            "{task} has no crewmate to carry on"
        ))));
    };
    let event = match row.status {
        TaskStatus::Stuck => TaskEvent::Continued,
        _ => TaskEvent::Released,
    };
    let to = match next_for(task, row.status, event) {
        Ok(to) => to,
        Err(r) => return Ok(Err(r)),
    };
    match reason {
        HoldReason::RepairLimit => {
            let mut state = load_state(conn, task)?;
            state.reset();
            save_state(conn, task, &state)?;
        }
        HoldReason::ReviewRoundLimit => store::set_review_rounds(conn, task, 0)?,
        HoldReason::Stuck | HoldReason::NeedsUser => {
            // "A stuck task while one of those limits stands": your answer resets them too.
            let mut state = load_state(conn, task)?;
            if state.held {
                state.reset();
                save_state(conn, task, &state)?;
            }
            if store::facts(conn, task)?
                .is_some_and(|f| f.review_rounds > qm_policy::review::MAX_RE_REVIEWS)
            {
                store::set_review_rounds(conn, task, 0)?;
            }
        }
    }
    task_store::set_status(conn, task, row.status, to, at)?;
    let message = messages::insert(
        conn,
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
    let _ = label;
    Ok(Ok(message))
}

impl DecisionService {
    #[allow(clippy::too_many_lines)]
    async fn perform(
        &self,
        held: &LoadedHeld,
        answer: &HeldAnswer,
    ) -> Result<HeldOutcome, HeldError> {
        let HeldPayload::Decision(payload) = &held.payload else {
            return Err(Refusal::new("this card isn't a decision").into());
        };
        let HeldAnswer::Choose { choice, words } = answer else {
            return Err(Refusal::new("this card is a decision; pick one of its choices").into());
        };
        check_choice(&held.choices, choice)?;
        let task = payload.task;
        let label = choice.label();
        let note = match choice {
            HeldChoice::RetryWithGuidance => {
                let Some(words) = words
                    .as_deref()
                    .map(str::trim)
                    .filter(|w| !w.is_empty())
                    .map(neutralise)
                else {
                    return Err(Refusal::new("Retry with my guidance needs your words").into());
                };
                let at = self.clock().now();
                let reason = payload.reason;
                let text = words.clone();
                let message = self
                    .db()
                    .call(move |conn| {
                        let tx = conn.transaction()?;
                        let out = release_with_words(&tx, task, reason, &text, label, at)?;
                        if out.is_ok() {
                            tx.commit()?;
                        }
                        Ok(out)
                    })
                    .await??;
                self.notify(&Notify::new());
                self.tasks().deliver_user_message(task, message);
                format!("You chose {label} on {task}'s card: “{words}”. {task} carries on.")
            }
            HeldChoice::StrongerModel { tier, .. } => {
                let started = self
                    .tasks()
                    .start_retry(task, *tier, words.clone())
                    .await
                    .map_err(|f| match f {
                        qm_tools::handler::ToolFailure::Refused(r) => HeldError::Refused(r),
                        qm_tools::handler::ToolFailure::Failed(m) => HeldError::Failed(m),
                    })?;
                let new = started.task;
                let model = started
                    .routing
                    .model
                    .as_ref()
                    .map_or_else(|| "waiting for a model".to_owned(), ToString::to_string);
                self.tasks()
                    .stop(task, Some(format!("retried as {new} on a stronger model")))
                    .await
                    .map_err(HeldError::Failed)?;
                let at = self.clock().now();
                let tier = *tier;
                self.db()
                    .call(move |conn| {
                        task_store::event(
                            conn,
                            RETRIED_EVENT,
                            task,
                            None,
                            &format!("{task} retried as {new} ({tier})"),
                            serde_json::json!({ "retry": new, "tier": tier }),
                            at,
                        )
                    })
                    .await?;
                self.changed();
                format!("Started {new} at {tier} ({model}) to retry {task}.")
            }
            HeldChoice::Discard => {
                self.tasks()
                    .discard(task)
                    .await
                    .map_err(HeldError::Failed)?;
                let at = self.clock().now();
                self.db()
                    .call(move |conn| {
                        task_store::event(
                            conn,
                            DISCARDED_EVENT,
                            task,
                            None,
                            &format!("{task} discarded by you"),
                            serde_json::json!({}),
                            at,
                        )
                    })
                    .await?;
                self.changed();
                format!("Discarded {task}.")
            }
            HeldChoice::Continue => {
                self.tasks()
                    .continue_after_stuck(task)
                    .await
                    .map_err(HeldError::Failed)?;
                format!("You chose Continue; {task} carries on.")
            }
            HeldChoice::DropTask => {
                self.tasks()
                    .stop(task, Some("dropped by the user".to_owned()))
                    .await
                    .map_err(HeldError::Failed)?;
                format!("You dropped {task}.")
            }
            other => {
                return Err(Refusal::new(format!(
                    "\"{}\" isn't a choice this build can carry out",
                    other.label()
                ))
                .into());
            }
        };
        let mut outcome = HeldOutcome::new(note);
        outcome.you = Some(YouLine {
            kind: CHOSE_EVENT,
            summary: format!("you chose {label} on {task}'s card"),
        });
        Ok(outcome)
    }
}
