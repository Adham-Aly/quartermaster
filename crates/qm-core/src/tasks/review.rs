//! A review's findings reach the user first under normal autonomy.
//!
//! Under normal autonomy a review's done report needs its static `report.html`
//! ([`crate::reports`]). The report moves the review to `waiting` on the user, with no supervisor
//! wake: `waiting_on` keeps `{"review": <report>, "wake": <text>}`, and the Actions panel lists
//! the review's card (Open, Go ahead). Only the user's go-ahead (the card's button, or
//! `approve_review` on a turn the user's message started) moves it to `reported` and gives the
//! supervisor the report. The supervisor then does with the findings what the user asked for when
//! they asked for the review: start a crewmate, pass them to a task, anything else, or nothing at
//! all if they didn't say. There is no default follow-up.
//!
//! Under auto autonomy, as with plans, there is no document and no wait: the report wakes the
//! supervisor at once. Switching a project to auto releases the reviews that wait on you.
//!
//! The app's own consequences of a Careful review's verdict (the pass that lets the builder push,
//! the round count, a hold) happen when the review reports, whatever the autonomy; only the
//! supervisor's part waits.

use super::{TaskService, failed, load_project, new_wake, refuse};
use crate::inbox::Inbox;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::Ack;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{Autonomy, Role, TaskKind, TaskStatus, WakeKind};
use qm_types::ids::{AgentId, ProjectId, ReportId, TaskId};

/// `events.kind` of a review's report put to the user.
pub const TO_USER_EVENT: &str = "task.review_to_user";
/// `events.kind` of the user's go-ahead on a review's findings.
pub const GO_AHEAD_EVENT: &str = "task.review_go_ahead";

/// The `waiting_on` key of a review waiting on the user's go-ahead.
const KEY: &str = "review";
/// The `waiting_on` key of the supervisor's wake text, kept until the go-ahead.
const WAKE: &str = "wake";

/// What a review waiting on the user keeps in `waiting_on`.
pub(crate) fn waiting_on(report: ReportId, wake: &str) -> serde_json::Value {
    serde_json::json!({ KEY: report.get(), WAKE: wake })
}

/// Whether `row` is a review waiting on the user's go-ahead.
pub fn awaits_go_ahead(row: &TaskRow) -> bool {
    row.kind == TaskKind::Review
        && row.status == TaskStatus::Waiting
        && row
            .waiting_on
            .as_ref()
            .is_some_and(|w| w.get(KEY).is_some())
}

/// Whether a steer reaches a review waiting on the user: it goes back to work.
pub(crate) fn is_review_wait(waiting_on: &serde_json::Value) -> bool {
    waiting_on.get(KEY).is_some()
}

impl TaskService {
    /// The autonomy of `row`'s project now.
    pub(crate) async fn live_autonomy(&self, row: &TaskRow) -> Result<Autonomy, ToolFailure> {
        let project = row.project;
        let task = row.id;
        self.db()
            .call(move |conn| load_project(conn, project))
            .await
            .map_err(|e| failed(&e))?
            .map(|p| p.autonomy)
            .ok_or_else(|| refuse(format!("{task}'s project is gone")))
    }
}

/// Who gives the go-ahead.
#[derive(Clone, Copy, Debug)]
pub enum GoAhead<'a> {
    /// An orchestrator's `approve_review` on a turn the user's message started.
    Agent(&'a Caller),
    /// The Go ahead button on the review's card.
    Card,
    /// The project switched to auto autonomy, where findings don't wait for the user.
    Autonomy,
    /// The review reported under auto autonomy: nothing waited.
    Auto,
}

/// What the supervisor is told to do with the findings, once they may act. There is no default
/// follow-up: only what the user asked for, or the app's own next step for a Careful review.
pub(crate) fn follow_up(task: TaskId, by: GoAhead<'_>) -> String {
    let why = match by {
        GoAhead::Autonomy => format!(
            "The project switched to auto autonomy, so {task}'s findings no longer wait for the user."
        ),
        GoAhead::Auto => format!("Under auto autonomy, {task}'s findings don't wait for the user."),
        GoAhead::Card => format!(
            "The user read {task}'s review report and pressed Go ahead on its card in the app."
        ),
        GoAhead::Agent(_) => format!("You recorded the user's go-ahead on {task}'s findings."),
    };
    format!(
        "{why} Do with the findings only what the user asked for when they asked for the review \
         (start a crewmate, pass them to a task, anything else they said) or what the app says \
         here; if neither says, do nothing with them. Then settle_task {task}."
    )
}

/// The user's go-ahead on `task`'s findings: the review moves to `reported` and its supervisor
/// gets the report and the follow-up, by a wake unless it gave the go-ahead itself (then in the
/// returned note).
pub(crate) async fn go_ahead(
    svc: &TaskService,
    by: GoAhead<'_>,
    task: TaskId,
) -> Result<String, ToolFailure> {
    let row = super::waits::load_task(svc, task).await?;
    if let GoAhead::Agent(caller) = by
        && caller.role == Role::Secondmate
    {
        super::waits::check_direct(svc, caller, &row).await?;
    }
    if row.kind != TaskKind::Review {
        return Err(refuse(format!("{task} isn't a review task")));
    }
    if !awaits_go_ahead(&row) {
        return Err(refuse(format!(
            "{task} isn't waiting for the user's go-ahead (it's {}); nothing to approve",
            row.status
        )));
    }
    let by_agent: Option<AgentId> = match by {
        GoAhead::Agent(caller) => Some(caller.agent),
        GoAhead::Card | GoAhead::Autonomy | GoAhead::Auto => None,
    };
    let supervisor = row.supervisor;
    let follow = follow_up(task, by);
    let at = svc.clock().now();
    let follow2 = follow.clone();
    let summary = match by {
        GoAhead::Autonomy | GoAhead::Auto => {
            format!("{task}'s findings went on under auto autonomy")
        }
        GoAhead::Agent(_) | GoAhead::Card => {
            format!("the user gave the go-ahead on {task}'s findings")
        }
    };
    let text = svc
        .write(move |tx, notify| {
            // Read again inside the change: another go-ahead or a steer may have come first.
            let Some(now) = task_store::load(tx, task)? else {
                return Ok(None);
            };
            if !awaits_go_ahead(&now) {
                return Ok(None);
            }
            let waiting = now.waiting_on.clone().unwrap_or_default();
            let report = waiting.get(KEY).and_then(serde_json::Value::as_u64);
            let reported = waiting
                .get(WAKE)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let text = if reported.is_empty() {
                follow2
            } else {
                format!("{reported} {follow2}")
            };
            task_store::set_waiting_on(tx, task, None)?;
            task_store::set_status(tx, task, TaskStatus::Waiting, TaskStatus::Reported, at)?;
            super::waits::release_waiters(tx, notify, task, TaskStatus::Reported, at)?;
            task_store::event(
                tx,
                GO_AHEAD_EVENT,
                task,
                by_agent,
                &summary,
                serde_json::json!({ "report": report }),
                at,
            )?;
            if by_agent != Some(supervisor) {
                let wake = new_wake(
                    supervisor,
                    WakeKind::Reported,
                    task,
                    format!("review_go_ahead:{task}:{}", report.unwrap_or_default()),
                    &text,
                    &format!("go-ahead on {task}'s review"),
                    serde_json::json!({ "report": report }),
                    at,
                );
                Inbox::post(tx, &wake, notify)?;
            }
            Ok(Some(text))
        })
        .await
        .map_err(|e| failed(&e))?;
    svc.changed();
    let Some(text) = text else {
        return Err(refuse(format!(
            "{task} isn't waiting for the user's go-ahead any more"
        )));
    };
    Ok(if by_agent == Some(supervisor) {
        format!("Go-ahead recorded. {text}")
    } else {
        format!("Go-ahead recorded. {follow}")
    })
}

/// The review go-ahead's tool: `approve_review`.
pub(crate) async fn approve_review(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
) -> Result<Ack, ToolFailure> {
    let note = go_ahead(svc, GoAhead::Agent(caller), task).await?;
    Ok(Ack { note })
}

/// Your autonomy change for `project`: under auto, its reviews waiting on you go on.
pub(crate) async fn autonomy_changed(svc: &TaskService, project: ProjectId) {
    let rows = svc
        .db()
        .call(move |conn| {
            let auto = load_project(conn, project)?.is_some_and(|p| p.autonomy == Autonomy::Auto);
            if !auto {
                return Ok(Vec::new());
            }
            let mut waiting = Vec::new();
            for (id, kind) in task_store::open_in_project(conn, project)? {
                if kind == TaskKind::Review
                    && let Some(row) = task_store::load(conn, id)?
                    && awaits_go_ahead(&row)
                {
                    waiting.push(id);
                }
            }
            Ok(waiting)
        })
        .await;
    let tasks = match rows {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::error!(error = %e, "couldn't read the reviews an autonomy change reaches");
            return;
        }
    };
    for task in tasks {
        if let Err(e) = go_ahead(svc, GoAhead::Autonomy, task).await {
            tracing::warn!(%task, error = %e.render(), "couldn't release a review under auto autonomy");
        }
    }
}
