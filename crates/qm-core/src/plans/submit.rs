//! A plan reply sent from the plan's page ([`super::endpoint`] hands over the body).
//!
//! The reply is checked against the plan's saved state, then claimed (one claim per round, under
//! the plan's write lock), delivered and finished:
//!
//! 1. **Claim**: `page_claim = round`, saved. A second request for the round, or a paste of it,
//!    is refused from here on.
//! 2. **Deliver**, in one transaction: the same supervisor message and `steer` wake a pasted
//!    reply makes ([`crate::tasks::waits::deliver_to_crewmate`]), the `task.plan_page_reply`
//!    event, and a one-line acknowledgement in the Quartermaster's chat.
//! 3. **Finish**: the round counts as answered (as a paste does), `page_sent` is set, the claim
//!    cleared, and the page rendered again as sent.
//!
//! A crash between claim and finish is settled at startup ([`recover`]): with the delivery's event
//! the round is finished, without it the claim is dropped. Delivery and its event share one
//! transaction, so a reply is never delivered twice or half-recorded.
//!
//! Nothing here logs the body, the key or the reply: only the task, round and outcome.

use super::{PageSent, PlanState, Stage};
use crate::tasks::TaskService;
use crate::tasks::lifecycle::{is_exiting, is_open};
use qm_plans::reply;
use qm_policy::markers::neutralise;
use qm_store::events;
use qm_store::messages::{self, NewMessage};
use qm_store::tasks as task_store;
use qm_types::domain::{MessageKind, MessageSource, PlanMode, TaskKind};
use qm_types::ids::TaskId;
use serde::Deserialize;
use std::fmt::Write as _;
use std::path::PathBuf;

/// `events.kind` of a plan reply sent from the page and delivered to the planner.
pub const PAGE_REPLY_EVENT: &str = "task.plan_page_reply";

/// How a submission went: the page shows `message` and acts on the status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Delivered to the planner.
    Sent,
    /// Not a submission the page makes.
    Invalid,
    /// The plan takes no more replies.
    Closed,
    /// The page is for another round or plan, or no longer open.
    Stale,
    /// The page's key doesn't match.
    Forbidden,
    /// The round already has its reply.
    Duplicate,
    /// The app couldn't pass it on; nothing was sent.
    Unavailable,
}

impl Status {
    /// The word the page reads.
    pub fn word(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Invalid => "invalid",
            Self::Closed => "closed",
            Self::Stale => "stale",
            Self::Forbidden => "forbidden",
            Self::Duplicate => "duplicate",
            Self::Unavailable => "unavailable",
        }
    }

    /// The HTTP status the endpoint answers with.
    pub fn http(self) -> u16 {
        match self {
            Self::Sent => 200,
            Self::Invalid => 400,
            Self::Forbidden => 403,
            Self::Duplicate => 409,
            Self::Closed | Self::Stale => 410,
            Self::Unavailable => 503,
        }
    }
}

/// The answer to a submission: a status and one sentence for the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: Status,
    pub message: String,
}

impl Answer {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// What the page posts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    v: u32,
    task: String,
    plan: String,
    round: u32,
    token: String,
    reply: String,
}

/// A submission that passed its checks.
struct Checked {
    task: TaskId,
    /// The plan's folder.
    dir: PathBuf,
    body: Body,
    reply: reply::PlanReply,
    /// What the planner receives.
    text: String,
}

const INVALID: &str = "This reply couldn't be read. Reload the page (⌘R), or use Copy reply.";
const CLOSED: &str = "This plan is no longer taking replies.";
const STALE: &str = "This page is out of date; reopen the plan from its card.";
const FORBIDDEN: &str = "This page can't send its reply; reopen the plan from its card, or use \
                         Copy reply.";
const UNAVAILABLE: &str = "Quartermaster couldn't pass it on; try again or Copy reply.";

/// Handles one submission's body, end to end.
pub async fn submit(svc: &TaskService, body: &[u8]) -> Answer {
    let checked = match check(svc, body).await {
        Ok(checked) => checked,
        Err(answer) => return answer,
    };
    let (task, round) = (checked.task, checked.body.round);
    let answer = claim_and_deliver(svc, checked).await;
    tracing::info!(%task, round, outcome = answer.status.word(), "plan reply from the page");
    answer
}

/// Every check before the claim, in the documented order; the first failure answers.
async fn check(svc: &TaskService, body: &[u8]) -> Result<Checked, Answer> {
    let invalid = || Answer::new(Status::Invalid, INVALID);
    let body: Body = serde_json::from_slice(body).map_err(|_| invalid())?;
    if body.v != 1 || body.reply.len() > reply::MAX_REPLY_CHARS + 512 {
        return Err(invalid());
    }
    let task: TaskId = body.task.parse().map_err(|_| invalid())?;

    let closed = || Answer::new(Status::Closed, CLOSED);
    let row = svc
        .db()
        .call(move |conn| task_store::load(conn, task))
        .await
        .map_err(|_| Answer::new(Status::Unavailable, UNAVAILABLE))?
        .ok_or_else(closed)?;
    if !is_open(row.status) || is_exiting(row.status) || row.kind != TaskKind::Plan {
        return Err(closed());
    }
    match svc.live_plan_mode(&row).await {
        Ok(Some(PlanMode::Interactive)) => {}
        Ok(_) | Err(_) => return Err(closed()),
    }
    let dir = svc.plan_dir(&row);
    let read = dir.clone();
    let state = tokio::task::spawn_blocking(move || super::load(&read))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .ok_or_else(closed)?;
    check_state(&state, &body)?;

    let (replies, malformed) = reply::find(&body.reply);
    let [one] = replies.as_slice() else {
        return Err(invalid());
    };
    if !malformed.is_empty()
        || !reply::only_replies(&body.reply)
        || one.task != body.task
        || one.plan_uid != body.plan
        || one.round != body.round
    {
        return Err(invalid());
    }
    let reply = one.clone();
    let text = neutralise(body.reply.trim());
    Ok(Checked {
        task,
        dir,
        body,
        reply,
        text,
    })
}

/// The checks against the plan's state, made before the claim and again under the write lock.
fn check_state(state: &PlanState, body: &Body) -> Result<(), Answer> {
    if state.uid != body.plan {
        return Err(Answer::new(Status::Stale, STALE));
    }
    if state.stage == Stage::Approved {
        return Err(Answer::new(Status::Closed, CLOSED));
    }
    if !state
        .page_token
        .as_ref()
        .is_some_and(|t| t.matches(&body.token))
    {
        return Err(Answer::new(Status::Forbidden, FORBIDDEN));
    }
    if body.round != state.round
        || !matches!(state.stage, Stage::WithUser | Stage::Final)
        || state.locked.is_some()
    {
        return Err(Answer::new(Status::Stale, STALE));
    }
    let round = body.round;
    if state.pending_reply && state.take_recommended {
        return Err(Answer::new(
            Status::Duplicate,
            format!(
                "Round {round} was approved with the planner's recommendations; there's nothing \
                 to send."
            ),
        ));
    }
    if state.sent_from_page()
        || state.last_answered_round >= round
        || state.pending_reply
        || state.page_claim.is_some()
    {
        return Err(Answer::new(
            Status::Duplicate,
            format!("Round {round} already has your reply."),
        ));
    }
    Ok(())
}

/// The claim, the delivery and the finish.
async fn claim_and_deliver(svc: &TaskService, checked: Checked) -> Answer {
    let Checked {
        task,
        dir,
        body,
        reply,
        text,
    } = checked;
    let round = body.round;
    let plan = body.plan.clone();
    // Checked again under the lock: a paste or another request may have answered the round.
    let claimed = tokio::task::spawn_blocking(move || {
        super::try_update(&dir, |s| {
            check_state(s, &body)?;
            s.page_claim = Some(round);
            Ok::<_, Answer>(())
        })
    })
    .await;
    match claimed {
        Ok(Ok(Some(Ok(_)))) => {}
        Ok(Ok(Some(Err(answer)))) => return answer,
        Ok(Ok(None)) => return Answer::new(Status::Closed, CLOSED),
        Ok(Err(_)) | Err(_) => return Answer::new(Status::Unavailable, UNAVAILABLE),
    }

    let (answered, open, sections) = reply.counts();
    let note = acknowledgement(task, round, answered, answered + open, sections);
    let seat = svc.deps().seat_agent;
    let at = svc.clock().now();
    let delivered = svc
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(None);
            };
            if !is_open(row.status) || is_exiting(row.status) {
                return Ok(None);
            }
            let delivered = crate::tasks::waits::deliver_to_crewmate(
                tx,
                notify,
                task,
                row.supervisor,
                &text,
                at,
            )?;
            let Some(message) = delivered.message else {
                return Ok(None);
            };
            task_store::event(
                tx,
                PAGE_REPLY_EVENT,
                task,
                None,
                &format!("the user sent {task}'s round {round} plan reply from its page"),
                serde_json::json!({
                    "task": task.to_string(),
                    "plan": plan,
                    "round": round,
                    "delivered": message,
                    "counts": { "answered": answered, "open": open, "sections": sections },
                }),
                at,
            )?;
            let ack = messages::insert(
                tx,
                &NewMessage {
                    agent: seat,
                    source: MessageSource::App,
                    kind: MessageKind::AppNote,
                    text: &note,
                    options: &[],
                    replies_to: None,
                    turn: None,
                    at,
                },
            )?;
            task_store::event(
                tx,
                events::PLAN_REPLY_NOTE,
                task,
                None,
                &note,
                serde_json::json!({ "message": ack, "task": task.to_string(), "round": round }),
                at,
            )?;
            Ok(Some(message))
        })
        .await;
    let Ok(Some(_)) = delivered.map_err(|e| {
        tracing::error!(%task, round, error = %e, "couldn't deliver a plan reply from the page");
    }) else {
        release(svc, task, round).await;
        return Answer::new(Status::Unavailable, UNAVAILABLE);
    };
    svc.supervisor_acted(task);

    let shown = crate::tasks::plan::local_time(at);
    finish(svc, task, round, shown.clone()).await;
    Answer::new(Status::Sent, format!("Sent to Quartermaster at {shown}."))
}

/// The chat's one-line acknowledgement: what the reply held, leaving out what it didn't (a final
/// round has no questions, so its reply is section notes only).
fn acknowledgement(
    task: TaskId,
    round: u32,
    answered: usize,
    of: usize,
    sections: usize,
) -> String {
    let mut parts = Vec::new();
    if of > 0 {
        parts.push(format!("{answered} of {of} answered"));
    }
    if sections > 0 {
        parts.push(format!(
            "{sections} section note{}",
            if sections == 1 { "" } else { "s" }
        ));
    }
    let mut text = format!("Plan reply sent from the page · {task} round {round}");
    if !parts.is_empty() {
        let _ = write!(text, " · {}", parts.join(", "));
    }
    text
}

/// Drops the claim on `round` after a delivery that didn't happen.
async fn release(svc: &TaskService, task: TaskId, round: u32) {
    let Ok(dir) = svc.paths(task).await.map(|p| p.plan) else {
        tracing::error!(%task, round, "couldn't find a plan to release a reply's claim");
        return;
    };
    let released = tokio::task::spawn_blocking(move || {
        super::update(&dir, |s| {
            if s.page_claim == Some(round) {
                s.page_claim = None;
            }
        })
    })
    .await;
    if !matches!(released, Ok(Ok(_))) {
        tracing::error!(%task, round, "couldn't release a plan reply's claim");
    }
}

/// Records a delivered reply to `round` in the plan's state, as a paste does, and renders the
/// page again as sent.
async fn finish(svc: &TaskService, task: TaskId, round: u32, at: String) {
    let Ok(dir) = svc.paths(task).await.map(|p| p.plan) else {
        // The reply was delivered; the next startup finishes the round from its event.
        tracing::error!(%task, round, "couldn't find the plan to record a reply sent from its page");
        return;
    };
    let project = svc.project_name_of(task).await;
    let port = svc.plan_port();
    let finished = tokio::task::spawn_blocking(move || {
        let saved = super::update(&dir, |s| finish_state(s, round, at))?;
        if saved.is_some()
            && let Err(why) = super::rerender(&dir, task, &project, port)
        {
            tracing::warn!(%task, %why, "couldn't show the plan's page as sent");
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    if !matches!(finished, Ok(Ok(()))) {
        // The reply was delivered; the next startup finishes the round from its event.
        tracing::error!(%task, round, "couldn't record a plan reply sent from the page");
    }
    svc.changed();
}

fn finish_state(state: &mut PlanState, round: u32, at: String) {
    if state.page_claim != Some(round) {
        return;
    }
    super::record_answer(state, round);
    state.page_sent = Some(PageSent { round, at });
    state.page_claim = None;
}

/// At startup: settles each plan whose page reply was claimed but not finished when the app
/// stopped. With its delivery's event the round is finished; without it the claim goes, and the
/// user can send again.
pub async fn recover(svc: &TaskService) {
    let data = svc.deps().data_dir.clone();
    let claimed: Vec<(TaskId, u32)> = tokio::task::spawn_blocking(move || super::api::live(&data))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(task, state, _)| Some((task, state.page_claim?)))
        .collect();
    for (task, round) in claimed {
        let delivered = svc
            .db()
            .call(move |conn| events::has_round(conn, PAGE_REPLY_EVENT, task, round))
            .await;
        match delivered {
            Ok(true) => {
                let at = delivered_at(svc, task, round).await;
                finish(svc, task, round, at).await;
                tracing::info!(%task, round, "finished a plan reply sent before the restart");
            }
            Ok(false) => {
                release(svc, task, round).await;
                tracing::info!(%task, round, "dropped an undelivered plan reply's claim");
            }
            Err(e) => tracing::error!(%task, round, error = %e, "couldn't settle a plan reply"),
        }
    }
}

/// When the reply to `round` was delivered, as shown, from its event.
async fn delivered_at(svc: &TaskService, task: TaskId, round: u32) -> String {
    let at = svc
        .db()
        .call(move |conn| {
            Ok(events::latest(conn, PAGE_REPLY_EVENT, 64)?
                .into_iter()
                .find(|e| {
                    e.task == Some(task)
                        && e.payload.get("round").and_then(serde_json::Value::as_u64)
                            == Some(u64::from(round))
                })
                .map(|e| e.at))
        })
        .await
        .ok()
        .flatten();
    crate::tasks::plan::local_time(at.unwrap_or_else(|| svc.clock().now()))
}
