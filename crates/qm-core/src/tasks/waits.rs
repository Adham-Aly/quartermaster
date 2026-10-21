//! Waiting, steering and linking.
//!
//! - `ask_decision` and `declare_wait` put the caller's task in a declared wait (a sign of life
//!   that pauses both watchdogs).
//! - `message_task` steers a direct report: the supervisor's message, marked, as a `steer` wake;
//!   it ends a decision wait and steers a reported task back to work.
//! - `link_tasks` records "blocks"; a running blocked task is told, and every waiter is woken when
//!   the task it waits on finishes (`release_waiters`, in the same transaction as that status
//!   change).
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
use qm_tools::defs::{Ack, DeliveryCheck, FileFollowupArgs, Linked, TaskUpdated};
use qm_tools::handler::{Caller, DeclaredWait, ToolFailure};
use qm_types::domain::{DeliveryMode, MessageKind, MessageSource, TaskStatus, WakeKind};
use qm_types::ids::TaskId;
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
    let kind = task_store::load(tx, task)?.map(|r| r.kind);
    if let Some(kind) = kind
        && blocker_finished(kind, status)
    {
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
    // A secondmate's scope: its crewmates' follow-ups stay inside it.
    let (scoped, project_id, project_name) = (*caller, project.id, project.name.clone());
    svc.db()
        .call(move |conn| {
            crate::secondmates::scope::check_project_in(conn, &scoped, project_id, &project_name)
        })
        .await
        .map_err(|e| failed(&e))??;
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
    let text = neutralise(text.trim());
    let at = svc.clock().now();
    let supervisor = caller.agent;
    let status = svc
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(TaskStatus::Closed);
            };
            let Some(crewmate) = agents::crewmate_of(tx, task)? else {
                return Ok(row.status);
            };
            let message = messages::insert(
                tx,
                &NewMessage {
                    agent: crewmate.id,
                    source: MessageSource::Supervisor,
                    kind: MessageKind::Message,
                    text: &text,
                    options: &[],
                    replies_to: None,
                    turn: None,
                    at,
                },
            )?;
            let waiting_on_decision = row.status == TaskStatus::Waiting
                && row
                    .waiting_on
                    .as_ref()
                    .is_some_and(|w| w.get("decision").is_some());
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
                Some(supervisor),
                &format!("{task} was steered by its supervisor"),
                serde_json::json!({ "message": message }),
                at,
            )?;
            let wake = new_wake(
                crewmate.id,
                WakeKind::Steer,
                task,
                format!("steer:{task}:{message}"),
                &text,
                "your supervisor wrote",
                serde_json::json!({ "message": message }),
                at,
            );
            Inbox::post(tx, &wake, notify)?;
            Ok(status)
        })
        .await
        .map_err(|e| failed(&e))?;
    svc.supervisor_acted(task);
    Ok(TaskUpdated {
        task,
        status,
        kept_ref: None,
    })
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
            task_store::event(
                tx,
                LINKED_EVENT,
                blocked,
                Some(agent),
                &format!("{blocked} now waits on {blocker}"),
                serde_json::json!({ "blocker": blocker, "blocked": blocked }),
                at,
            )?;
            let blocker_now = task_store::load(tx, blocker)?;
            let finished = blocker_now
                .as_ref()
                .is_some_and(|b| blocker_finished(b.kind, b.status));
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
                    format!("blocked_by:{blocked}:{blocker}"),
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
