//! Holding a task for your decision.
//!
//! [`hold`] runs in the caller's transaction (the final checks verdict, a review's report): the
//! task goes `held`, one decision card is written with the app's fixed choices
//! (`qm_policy::holds`), and the supervisor gets a `task_held` wake asking for its view with
//! `present_hold`. A task has at most one pending decision; a second hold writes nothing.

use crate::held::HeldService;
use crate::inbox::{Inbox, Notify};
use crate::tasks::lifecycle::{TaskEvent, next_for};
use crate::tasks::{load_project, new_wake, start::current_tier};
use jiff::Timestamp;
use qm_auth::LoginHealth;
use qm_policy::holds::{card_title, decision_choices};
use qm_store::StoreError;
use qm_store::agents;
use qm_store::decisions as store;
use qm_store::held::{DecisionPayload, HeldPayload, HoldReason, NewHeld, load_held};
use qm_store::tasks as task_store;
use qm_types::domain::{TaskStatus, Tier, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::{HeldActionId, TaskId};
use rusqlite::Connection;

/// `events.kind` of a hold.
pub const HELD_EVENT: &str = "task.held";

/// What a decision card is written from: the tier the task ran at and its model (its routing
/// decision), the tiers with a usable model now, its escalations, its project and title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldFacts {
    pub tier: Tier,
    pub model: String,
    pub usable: Vec<Tier>,
    pub escalations: u8,
    pub project: String,
    pub title: String,
}

impl HoldFacts {
    /// The facts for `task` now; `None` if it doesn't exist.
    pub fn load(
        conn: &Connection,
        task: TaskId,
        health: &LoginHealth,
        now: Timestamp,
    ) -> Result<Option<Self>, StoreError> {
        let Some(row) = task_store::load(conn, task)? else {
            return Ok(None);
        };
        let tier = current_tier(conn, task)?;
        let model = agents::crewmate_of(conn, task)?
            .and_then(|a| a.model)
            .map_or_else(|| "no model".to_owned(), |m| m.to_string());
        let project = load_project(conn, row.project)?.map_or_else(String::new, |p| p.name);
        let usable = crate::routing::usable_tiers(conn, health, now)?;
        Ok(Some(Self {
            tier,
            model,
            usable,
            escalations: row.escalations,
            project,
            title: row.title,
        }))
    }
}

/// The hold's reason in a few words, for the event, the wake and the context line: "3 repair
/// attempts used".
pub fn reason_words(reason: HoldReason) -> String {
    match reason {
        HoldReason::RepairLimit => format!(
            "{} repair attempts used",
            qm_policy::repair::MAX_FAILED_REPAIRS
        ),
        HoldReason::ReviewRoundLimit => {
            "the review still has findings after 2 re-reviews".to_owned()
        }
        HoldReason::Stuck => "stuck".to_owned(),
        HoldReason::NeedsUser => "the review is blocked".to_owned(),
    }
}

/// Holds `task` for your decision, in the caller's transaction. `Ok(None)`: a decision is already
/// pending for the task, so nothing more is written. A task that can't be held from its status is
/// refused.
#[allow(clippy::too_many_arguments)]
pub fn hold(
    conn: &Connection,
    task: TaskId,
    reason: HoldReason,
    detail: &str,
    facts: &HoldFacts,
    notify: &mut Notify,
    at: Timestamp,
) -> Result<Result<Option<HeldActionId>, Refusal>, StoreError> {
    let Some(row) = task_store::load(conn, task)? else {
        return Ok(Err(Refusal::new(format!("{task} doesn't exist"))));
    };
    if store::pending_for(conn, task)?.is_some() {
        return Ok(Ok(None));
    }
    let to = match next_for(task, row.status, TaskEvent::Held) {
        Ok(to) => to,
        Err(r) => return Ok(Err(r)),
    };
    task_store::set_status(conn, task, row.status, to, at)?;
    let choices = decision_choices(facts.tier, &facts.usable, facts.escalations);
    let title = card_title(task, &facts.title, &facts.project, facts.tier, &facts.model);
    let held = HeldService::insert(
        conn,
        &NewHeld {
            payload: HeldPayload::Decision(DecisionPayload {
                task,
                reason,
                detail: detail.to_owned(),
                summary: None,
            }),
            choices,
            title,
            task: Some(task),
            asked_by: Some(row.supervisor),
            at,
        },
    )?;
    let words = reason_words(reason);
    task_store::event(
        conn,
        HELD_EVENT,
        task,
        None,
        &format!("{task} held: {words}"),
        serde_json::json!({
            "held": held,
            "reason": reason,
            "detail": detail,
            "project": facts.project,
            "tier": facts.tier,
            "model": facts.model,
        }),
        at,
    )?;
    let text = format!(
        "The app is holding {task} ({words}). The user has a decision card with the fixed \
         choices. Add your view with present_hold — two or three sentences: what was tried and \
         what you think is going on. Don't choose for the user."
    );
    let wake = new_wake(
        row.supervisor,
        WakeKind::TaskHeld,
        task,
        format!("task_held:{task}:{held}"),
        &text,
        &format!("{task} held"),
        serde_json::json!({ "held": held }),
        at,
    );
    Inbox::post(conn, &wake, notify)?;
    Ok(Ok(Some(held)))
}

/// A steer or your direct message to a stuck crewmate: its pending stuck notice no longer applies.
/// Returns whether one was superseded.
pub fn supersede_stuck(conn: &Connection, task: TaskId, at: Timestamp) -> Result<bool, StoreError> {
    let Some(held) = store::pending_for(conn, task)? else {
        return Ok(false);
    };
    let row = load_held(conn, held)?;
    let stuck = matches!(
        &row.payload,
        HeldPayload::Decision(DecisionPayload {
            reason: HoldReason::Stuck,
            ..
        })
    );
    if !stuck {
        return Ok(false);
    }
    HeldService::supersede_with(conn, held, Some(task), "you stepped in", at)
}

/// Whether `status` shows a decision is with you (held, or stuck with a notice).
pub fn is_holding(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Held | TaskStatus::Stuck)
}
