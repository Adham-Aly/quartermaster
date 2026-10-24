//! Waiting, steering and dependency links.
//!
//! - `ask_decision` and `declare_wait` put the caller's task in a declared wait (a sign of life
//!   that pauses both watchdogs).
//! - `message_task` steers a direct report: the supervisor's message, marked, as a `steer` wake;
//!   it ends a decision wait and steers a reported task back to work.
//! - `link_tasks` records "blocks"; a running blocked task is told, and every waiter is woken when
//!   the task it waits on finishes (`release_waiters`, in the same transaction as that status
//!   change).
//! - `unlink_tasks` removes one dependency, ends only its matching declared wait and tells the
//!   blocked crewmate; other dependencies and task/decision waits remain intact.
//! - `file_followup` proposes a task to the supervisor; nothing starts.
//! - `delivery_check` answers from the same probe the start gate uses.

use super::delivery_probe::delivery_check as check_for_mode;
use super::lifecycle::{TaskEvent, is_exiting, is_finished, is_open, next_for};
use super::report::callers_task;
use super::start::blocker_finished;
use super::{TaskService, active_by_name, failed, load_project, new_wake, refuse};
use crate::inbox::{Inbox, Notify};
use jiff::Timestamp;
use qm_policy::markers::neutralise;
use qm_policy::scope::check_direct_report;
use qm_store::StoreError;
use qm_store::agents;
use qm_store::messages::{self, NewMessage};
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::{Ack, DeliveryCheck, FileFollowupArgs, Linked, TaskUpdated, Unlinked};
use qm_tools::handler::{Caller, DeclaredWait, ToolFailure};
use qm_types::domain::{DeliveryMode, MessageKind, MessageSource, TaskStatus, WakeKind};
use qm_types::ids::{AgentId, MessageId, TaskId};
use rusqlite::Connection;
use std::fmt::Write as _;

/// `events.kind` of a decision question.
pub const DECISION_EVENT: &str = "task.decision_asked";
/// `events.kind` of a declared wait.
pub const WAITING_EVENT: &str = "task.waiting";
/// `events.kind` of the supervisor's message.
pub const STEERED_EVENT: &str = "task.steered";
/// `events.kind` of a link.
pub const LINKED_EVENT: &str = "task.linked";
/// `events.kind` of a link removal.
pub const UNLINKED_EVENT: &str = "task.unlinked";
/// `events.kind` of a follow-up proposal.
pub const FOLLOWUP_EVENT: &str = "task.followup_filed";
/// `events.kind` of a delivery check.
pub const DELIVERY_EVENT: &str = "task.delivery_checked";

/// Loads a task the caller names, or refuses.
pub(crate) async fn load_task(svc: &TaskService, task: TaskId) -> Result<TaskRow, ToolFailure> {
    svc.db()
        .call(move |conn| task_store::load(conn, task))
        .await
        .map_err(|e| failed(&e))?
        .ok_or_else(|| refuse(format!("{task} doesn't exist")))
}

/// "Direct report only".
pub(crate) async fn check_direct(
    svc: &TaskService,
    caller: &Caller,
    row: &TaskRow,
) -> Result<(), ToolFailure> {
    if caller.agent == row.supervisor {
        return Ok(());
    }
    let owner = match svc.supervisor_label(row.supervisor).await.as_str() {
        "quartermaster" => "the Quartermaster".to_owned(),
        other => other.to_owned(),
    };
    check_direct_report(caller.agent, row.id, row.supervisor, &owner)?;
    Ok(())
}

/// When `task` reaches `status` (`reported`, `landed` or `closed`), inside that change's
/// transaction: every task in a declared wait on it goes back to running with a `wait_over`
/// wake, and every live task it blocks is told it finished.
pub(crate) fn release_waiters(
    tx: &Connection,
    notify: &mut Notify,
    task: TaskId,
    status: TaskStatus,
    at: Timestamp,
) -> Result<(), StoreError> {
    if !is_finished(status) {
        return Ok(());
    }
    let text = format!("{task} finished: {status}.");
    let short = format!("{task} finished");
    for (waiter, agent) in task_store::waiting_on_task(tx, task)? {
        task_store::set_status(tx, waiter, TaskStatus::Waiting, TaskStatus::Running, at)?;
        if let Some(agent) = agent {
            let wake = new_wake(
                agent,
                WakeKind::WaitOver,
                waiter,
                format!("wait_over:{waiter}:{task}:{status}"),
                &text,
                &short,
                serde_json::json!({ "other": task.to_string() }),
                at,
            );
            Inbox::post(tx, &wake, notify)?;
        }
    }
    if blocker_finished(tx, task)? {
        for (blocked, blocked_status, agent) in task_store::blocked_by(tx, task)? {
            let live = is_open(blocked_status)
                && !is_exiting(blocked_status)
                && blocked_status != TaskStatus::Queued;
            if let (true, Some(agent)) = (live, agent) {
                let wake = new_wake(
                    agent,
                    WakeKind::WaitOver,
                    blocked,
                    format!("wait_over:{blocked}:{task}:{status}"),
                    &text,
                    &short,
                    serde_json::json!({ "other": task.to_string() }),
                    at,
                );
                Inbox::post(tx, &wake, notify)?;
            }
        }
    }
    Ok(())
}

/// `ask_decision`.
pub(crate) async fn ask_decision(
    svc: &TaskService,
    caller: &Caller,
    question: String,
    options: Vec<String>,
) -> Result<Ack, ToolFailure> {
    let row = callers_task(svc, caller).await?;
    let task = row.id;
    let to = next_for(task, row.status, TaskEvent::Waited)?;
    let at = svc.clock().now();
    let agent = caller.agent;
    let from = row.status;
    let supervisor = row.supervisor;
    svc.write(move |tx, notify| {
        let event = task_store::event(
            tx,
            DECISION_EVENT,
            task,
            Some(agent),
            &format!("{task} asked its supervisor for a decision"),
            serde_json::json!({ "question": question, "options": options }),
            at,
        )?;
        task_store::set_status(tx, task, from, to, at)?;
        task_store::set_waiting_on(
            tx,
            task,
            Some(&serde_json::json!({ "decision": event.get() })),
        )?;
        let mut text = format!("{task} asks: {}", question.trim());
        if !options.is_empty() {
            let _ = write!(text, " Options: {}.", options.join(" · "));
        }
        text.push_str(" Answer with message_task.");
        let wake = new_wake(
            supervisor,
            WakeKind::DecisionAsked,
            task,
            format!("decision:{task}:{event}"),
            &text,
            &format!("{task} asked for a decision"),
            serde_json::json!({ "event": event }),
            at,
        );
        Inbox::post(tx, &wake, notify).map(drop)
    })
    .await
    .map_err(|e| failed(&e))?;
    Ok(Ack {
        note: "Asked. Your supervisor will answer; end your turn and wait.".to_owned(),
    })
}

/// `declare_wait`.
pub(crate) async fn declare_wait(
    svc: &TaskService,
    caller: &Caller,
    wait: DeclaredWait,
) -> Result<Ack, ToolFailure> {
    let row = callers_task(svc, caller).await?;
    let task = row.id;
    let (waiting_on, summary, note) = match &wait {
        DeclaredWait::Task(other) => {
            if *other == task {
                return Err(refuse("a task can't wait on itself"));
            }
            let target = load_task(svc, *other).await?;
            if is_finished(target.status) {
                return Err(refuse(format!(
                    "{other} has already finished ({}); carry on",
                    target.status
                )));
            }
            if is_exiting(target.status) {
                return Err(refuse(format!(
                    "{other} is ending ({}); it won't finish, so don't wait on it",
                    target.status
                )));
            }
            (
                serde_json::json!({ "task": other.to_string() }),
                format!("{task} waits on {other}"),
                format!("Waiting on {other}; you'll be woken when it finishes."),
            )
        }
        DeclaredWait::External(what) => (
            serde_json::json!({ "external": what }),
            format!("{task} waits on {what}"),
            "Waiting. Your next turn ends the wait, whatever starts it.".to_owned(),
        ),
    };
    let to = next_for(task, row.status, TaskEvent::Waited)?;
    let at = svc.clock().now();
    let agent = caller.agent;
    let from = row.status;
    svc.write(move |tx, _| {
        task_store::set_status(tx, task, from, to, at)?;
        task_store::set_waiting_on(tx, task, Some(&waiting_on))?;
        task_store::event(
            tx,
            WAITING_EVENT,
            task,
            Some(agent),
            &summary,
            waiting_on.clone(),
            at,
        )
        .map(drop)
    })
    .await
    .map_err(|e| failed(&e))?;
    Ok(Ack { note })
}

/// `file_followup`: a proposal for the supervisor; nothing starts.
pub(crate) async fn file_followup(
    svc: &TaskService,
    caller: &Caller,
    followup: FileFollowupArgs,
) -> Result<Ack, ToolFailure> {
    let row = callers_task(svc, caller).await?;
    let task = row.id;
    let project_id = row.project;
    let named = followup.project.clone();
    let project = svc
        .db()
        .call(move |conn| match named {
            Some(name) => active_by_name(conn, &name),
            None => load_project(conn, project_id),
        })
        .await
        .map_err(|e| failed(&e))?;
    let Some(project) = project else {
        return Err(refuse(format!(
            "There's no project called {}",
            followup.project.as_deref().unwrap_or("")
        )));
    };
    let at = svc.clock().now();
    let agent = caller.agent;
    let supervisor = row.supervisor;
    svc.write(move |tx, notify| {
        let event = task_store::event(
            tx,
            FOLLOWUP_EVENT,
            task,
            Some(agent),
            &format!("{task} proposed a follow-up in {}: {}", project.name, followup.title),
            serde_json::json!({
                "title": followup.title,
                "goal": followup.goal,
                "kind": followup.kind,
                "project": project.name,
            }),
            at,
        )?;
        let text = format!(
            "{task} proposes a follow-up in {} ({}): {}. {} Start it with start_crewmate if it's part of what the user asked.",
            project.name, followup.kind, followup.title, followup.goal
        );
        let wake = new_wake(
            supervisor,
            WakeKind::FollowupFiled,
            task,
            format!("followup:{task}:{event}"),
            &text,
            &format!("{task} proposed a follow-up"),
            serde_json::json!({ "event": event }),
            at,
        );
        Inbox::post(tx, &wake, notify).map(drop)
    })
    .await
    .map_err(|e| failed(&e))?;
    Ok(Ack {
        note: "Filed for your supervisor as a proposal; carry on with your own task.".to_owned(),
    })
}

/// `delivery_check`.
pub(crate) async fn delivery_check(
    svc: &TaskService,
    caller: &Caller,
) -> Result<DeliveryCheck, ToolFailure> {
    let row = callers_task(svc, caller).await?;
    let task = row.id;
    let project_id = row.project;
    let project = svc
        .db()
        .call(move |conn| load_project(conn, project_id))
        .await
        .map_err(|e| failed(&e))?
        .ok_or_else(|| refuse(format!("{task}'s project is gone")))?;
    let needs_probe = match row.delivery {
        DeliveryMode::DirectPr => true,
        DeliveryMode::BranchOnly => project.github_remote.is_some(),
        DeliveryMode::BuildInPlace | DeliveryMode::Folder => false,
    };
    let probed = if needs_probe {
        Some(svc.deps().probe.probe(&project, &project.path).await)
    } else {
        None
    };
    let check = check_for_mode(row.delivery, project.github_remote.is_some(), probed);
    let at = svc.clock().now();
    let agent = caller.agent;
    let gaps = check.gaps.clone();
    let _ = svc
        .write(move |tx, _| {
            task_store::event(
                tx,
                DELIVERY_EVENT,
                task,
                Some(agent),
                &format!("{task} checked delivery: {} gaps", gaps.len()),
                serde_json::json!({ "gaps": gaps }),
                at,
            )
            .map(drop)
        })
        .await;
    Ok(check)
}

/// `message_task`.
pub(crate) async fn message_task(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
    text: String,
) -> Result<TaskUpdated, ToolFailure> {
    let row = load_task(svc, task).await?;
    check_direct(svc, caller, &row).await?;
    if !is_open(row.status) || is_exiting(row.status) {
        return Err(refuse(format!(
            "{task} is {}; there's no crewmate to message",
            row.status
        )));
    }
    let plan_note = plan_replies(svc, caller, &row, &text).await?;
    let text = neutralise(text.trim());
    let at = svc.clock().now();
    let supervisor = caller.agent;
    let status = svc
        .write(move |tx, notify| {
            deliver_to_crewmate(tx, notify, task, supervisor, &text, at).map(|d| d.status)
        })
        .await
        .map_err(|e| failed(&e))?;
    svc.supervisor_acted(task);
    if let Some(note) = plan_note {
        tracing::info!(%task, "{note}");
    }
    Ok(TaskUpdated { task, status })
}

/// A supervisor's message delivered to a task's crewmate.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Delivered {
    /// The task's status after it.
    pub status: TaskStatus,
    /// The message, `None` when the task has no crewmate (or is gone).
    pub message: Option<MessageId>,
}

/// Delivers `text` (already neutralised) to `task`'s crewmate as a message from its supervisor
/// `from`, in the caller's transaction: the message, the `steer` wake, and the status change a
/// steer makes (a decision or plan wait, or a report, back to running; review or stuck on). The
/// one path for `message_task` and a reply sent from a plan's page.
pub(crate) fn deliver_to_crewmate(
    tx: &Connection,
    notify: &mut Notify,
    task: TaskId,
    from: AgentId,
    text: &str,
    at: Timestamp,
) -> Result<Delivered, StoreError> {
    let Some(row) = task_store::load(tx, task)? else {
        return Ok(Delivered {
            status: TaskStatus::Closed,
            message: None,
        });
    };
    let Some(crewmate) = agents::crewmate_of(tx, task)? else {
        return Ok(Delivered {
            status: row.status,
            message: None,
        });
    };
    let message = messages::insert(
        tx,
        &NewMessage {
            agent: crewmate.id,
            source: MessageSource::Supervisor,
            kind: MessageKind::Message,
            text,
            options: &[],
            replies_to: None,
            turn: None,
            at,
        },
    )?;
    // A crewmate waiting on a decision, a planner on the user's reply to its page, or a reviewer
    // on the user's go-ahead (steered back to work, it reports again).
    let waiting_on_decision = row.status == TaskStatus::Waiting
        && row.waiting_on.as_ref().is_some_and(|w| {
            w.get("decision").is_some()
                || w.get("plan").is_some()
                || super::review::is_review_wait(w)
        });
    let mut status = row.status;
    if waiting_on_decision || row.status == TaskStatus::Reported {
        task_store::set_status(tx, task, row.status, TaskStatus::Running, at)?;
        status = TaskStatus::Running;
    }
    // The review's findings passed to a builder in review, or a steer to a stuck crewmate,
    // whose pending stuck notice no longer applies.
    if matches!(row.status, TaskStatus::InReview | TaskStatus::Stuck)
        && let Ok(to) = next_for(task, row.status, TaskEvent::Steered)
    {
        task_store::set_status(tx, task, row.status, to, at)?;
        status = to;
        if row.status == TaskStatus::Stuck {
            crate::decisions::hold::supersede_stuck(tx, task, at)?;
        }
    }
    task_store::event(
        tx,
        STEERED_EVENT,
        task,
        Some(from),
        &format!("{task} was steered by its supervisor"),
        serde_json::json!({ "message": message }),
        at,
    )?;
    let wake = new_wake(
        crewmate.id,
        WakeKind::Steer,
        task,
        format!("steer:{task}:{message}"),
        text,
        "your supervisor wrote",
        serde_json::json!({ "message": message }),
        at,
    );
    Inbox::post(tx, &wake, notify)?;
    Ok(Delivered {
        status,
        message: Some(message),
    })
}

/// The user's plan replies in a message to a task, checked and recorded; a message without one
/// passes untouched.
async fn plan_replies(
    svc: &TaskService,
    caller: &Caller,
    row: &TaskRow,
    text: &str,
) -> Result<Option<String>, ToolFailure> {
    if !text.contains(qm_plans::reply::OPEN) {
        return Ok(None);
    }
    let (replies, malformed) = qm_plans::reply::find(text);
    if let Some(bad) = malformed.first() {
        return Err(refuse(match bad {
            qm_plans::reply::Malformed::Unclosed => {
                "that plan reply has no closing line; tell the user to copy it again from the page"
                    .to_owned()
            }
            qm_plans::reply::Malformed::TooLong => format!(
                "that plan reply is over {} characters; tell the user to shorten it",
                qm_plans::reply::MAX_REPLY_CHARS
            ),
        }));
    }
    let user_text = match caller.turn.and_then(|t| t.started_by.message()) {
        Some(message) => svc
            .db()
            .call(move |conn| messages::text(conn, message))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    super::plan::record_replies(svc, row, &replies, user_text.as_deref()).await
}

/// `link_tasks`.
pub(crate) async fn link_tasks(
    svc: &TaskService,
    caller: &Caller,
    blocker: TaskId,
    blocked: TaskId,
) -> Result<Linked, ToolFailure> {
    if blocker == blocked {
        return Err(refuse(format!("{blocker} can't block itself")));
    }
    let blocker_row = load_task(svc, blocker).await?;
    let blocked_row = load_task(svc, blocked).await?;
    for row in [&blocker_row, &blocked_row] {
        if !is_open(row.status) {
            return Err(refuse(format!("{} is closed", row.id)));
        }
    }
    check_direct(svc, caller, &blocked_row).await?;
    // Both tasks are the caller's direct reports.
    check_direct(svc, caller, &blocker_row).await?;
    let at = svc.clock().now();
    let agent = caller.agent;
    let outcome = svc
        .write(move |tx, notify| {
            if task_store::reaches(tx, blocked, blocker)? {
                return Ok(Err(format!(
                    "{blocker} already waits on {blocked}, so linking them this way would make a cycle"
                )));
            }
            if !task_store::insert_link(tx, blocker, blocked, at)? {
                return Ok(Ok(()));
            }
            let event = task_store::event(
                tx,
                LINKED_EVENT,
                blocked,
                Some(agent),
                &format!("{blocked} now waits on {blocker}"),
                serde_json::json!({ "blocker": blocker, "blocked": blocked }),
                at,
            )?;
            let finished = blocker_finished(tx, blocker)?;
            let live = matches!(
                blocked_row.status,
                TaskStatus::Preparing
                    | TaskStatus::Running
                    | TaskStatus::Waiting
                    | TaskStatus::Held
                    | TaskStatus::Stuck
                    | TaskStatus::Reported
                    | TaskStatus::InReview
            );
            if live
                && !finished
                && let Some(crewmate) = agents::crewmate_of(tx, blocked)?
            {
                let text = format!(
                    "Your task now waits on {blocker} ({}). Don't deliver before it finishes; you'll be woken when it does.",
                    blocker_row.title
                );
                let wake = new_wake(
                    crewmate.id,
                    WakeKind::BlockedBy,
                    blocked,
                    format!("blocked_by:{blocked}:{blocker}:{event}"),
                    &text,
                    &format!("{blocked} waits on {blocker}"),
                    serde_json::json!({ "other": blocker.to_string() }),
                    at,
                );
                Inbox::post(tx, &wake, notify)?;
            }
            Ok(Ok(()))
        })
        .await
        .map_err(|e| failed(&e))?;
    outcome.map_err(refuse)?;
    svc.supervisor_acted(blocked);
    Ok(Linked { blocker, blocked })
}

/// Removes a dependency without treating the blocker as delivered or releasing its other waiters.
pub(crate) async fn unlink_tasks(
    svc: &TaskService,
    caller: &Caller,
    blocker: TaskId,
    blocked: TaskId,
) -> Result<Unlinked, ToolFailure> {
    if blocker == blocked {
        return Err(refuse(format!("{blocker} can't block itself")));
    }
    // Unlike linking, removal must work even after either endpoint has been closed.
    let blocker_row = load_task(svc, blocker).await?;
    let blocked_row = load_task(svc, blocked).await?;
    check_direct(svc, caller, &blocker_row).await?;
    check_direct(svc, caller, &blocked_row).await?;
    let at = svc.clock().now();
    let agent = caller.agent;
    svc.write(move |tx, notify| {
        if !task_store::remove_link(tx, blocker, blocked)? {
            return Ok(());
        }
        let event = task_store::event(
            tx,
            UNLINKED_EVENT,
            blocked,
            Some(agent),
            &format!("{blocked} no longer depends on {blocker}"),
            serde_json::json!({ "blocker": blocker, "blocked": blocked }),
            at,
        )?;
        if let Some(row) = task_store::load(tx, blocked)? {
            if row.status == TaskStatus::Waiting
                && row.waiting_on.as_ref().and_then(|w| w.get("task"))
                    == Some(&serde_json::json!(blocker))
            {
                task_store::set_status(tx, blocked, TaskStatus::Waiting, TaskStatus::Running, at)?;
            }
            if is_open(row.status)
                && !is_exiting(row.status)
                && !matches!(
                    row.status,
                    TaskStatus::Queued | TaskStatus::Landed | TaskStatus::Settling
                )
                && let Some(crewmate) = agents::crewmate_of(tx, blocked)?
            {
                let text = format!(
                    "Your supervisor removed the dependency on {blocker}. It no longer blocks your task; other dependencies and task/decision waits still apply. An external wait ends on your next turn as usual. This does not mean {blocker} delivered."
                );
                let wake = new_wake(
                    crewmate.id,
                    WakeKind::WaitOver,
                    blocked,
                    format!("unlinked:{blocked}:{blocker}:{event}"),
                    &text,
                    &format!("{blocked} no longer depends on {blocker}"),
                    serde_json::json!({ "other": blocker.to_string() }),
                    at,
                );
                Inbox::post(tx, &wake, notify)?;
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| failed(&e))?;
    svc.supervisor_acted(blocked);
    Ok(Unlinked { blocker, blocked })
}
