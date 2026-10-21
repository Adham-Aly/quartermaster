//! Startup and resume.
//!
//! [`reconcile`] runs before the seat opens (step 4) and deletes nothing: crewmate turns still
//! running become interrupted with their wakes released, live crewmates are marked stopped, task
//! worktrees are matched against tasks, and anything that needs a look is one event each and one
//! note in the Bridge. [`catch_up`] runs once the seat is open (step 6): side exits finish their
//! retirement, queued tasks go back to the re-route tick, preparing tasks redo phase C,
//! interrupted crewmates resume with the app-marked `restarted` note, and every other open task
//! gets its runner back.

use super::close::{Exit, retire};
use super::lifecycle::has_worktree;
use super::ports::label as port_label;
use super::runner::RunnerStart;
use super::{TaskService, active_projects, load_project, new_wake};
use crate::inbox::Inbox;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_git::{MissingWorktree, ReconcileProject, ReconcileReport, reconcile as git_reconcile};
use qm_store::StoreError;
use qm_store::agents::{self, InterruptedCrewTurn};
use qm_store::events::{self, NewEvent};
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::turns::{cut_short, mark_recovered};
use qm_types::domain::{DeliveryMode, TaskStatus, TurnStatus, WakeKind};
use qm_types::ids::{PortBlock, ProjectId, TaskId};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// `events.kind` of a task resumed (or not) after a restart.
pub const RESTARTED_EVENT: &str = "task.restarted";
/// How far back closed tasks' ports are checked for leftovers.
pub const PORTS_LOOKBACK: Duration = Duration::from_hours(24 * 7);
/// The most closed tasks whose ports are checked.
pub const PORTS_CHECKED: usize = 50;

/// What reconcile found, for catch-up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub interrupted: Vec<InterruptedCrewTurn>,
    pub missing: Vec<MissingWorktree>,
}

/// Why startup's reconcile failed.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the task worktrees couldn't be checked: {0}")]
    Git(String),
}

/// Step 4 (before the seat opens). Deletes nothing.
pub async fn reconcile(svc: &TaskService) -> Result<ReconcileOutcome, StartupError> {
    let at = svc.clock().now();
    let interrupted = svc
        .db()
        .call(move |conn| {
            let interrupted = agents::interrupt_running_crew_turns(conn, at)?;
            agents::stop_live_crewmates(conn)?;
            Ok(interrupted)
        })
        .await?;

    let report = worktree_report(svc).await?;

    let mut lines = findings(&report);

    // Ports of recently closed tasks something still listens on.
    let since = at
        .checked_sub(jiff::SignedDuration::try_from(PORTS_LOOKBACK).unwrap_or_default())
        .unwrap_or(at);
    let closed = svc
        .db()
        .call(move |conn| task_store::recently_closed_ports(conn, since, PORTS_CHECKED))
        .await?;
    let sup = std::sync::Arc::clone(&svc.deps().supervisor);
    let busy = tokio::task::spawn_blocking(move || {
        closed
            .into_iter()
            .filter(|(_, base)| {
                sup.listeners(PortBlock { base: *base }.ports())
                    .is_ok_and(|l| !l.is_empty())
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    for (task, base) in &busy {
        lines.push((
            "reconcile.ports_in_use",
            Some(*task),
            format!(
                "something is still listening on {}, {task}'s ports",
                port_label(PortBlock { base: *base })
            ),
        ));
    }

    if !lines.is_empty() {
        let records = lines.clone();
        svc.write(move |tx, _| {
            for (kind, task, summary) in &records {
                // A task branch can name a task this store never had (made by another install).
                let task = match task {
                    Some(t) if task_store::load(tx, *t)?.is_some() => Some(*t),
                    _ => None,
                };
                events::insert(
                    tx,
                    &NewEvent {
                        kind,
                        agent: None,
                        task,
                        turn: None,
                        summary,
                        payload: serde_json::json!({}),
                        at,
                    },
                )?;
            }
            Ok(())
        })
        .await?;
        let n = lines.len();
        let (things, need) = if n == 1 {
            ("thing", "needs")
        } else {
            ("things", "need")
        };
        svc.app_note(&format!(
            "Startup check: {n} {things} in your task worktrees {need} a look; nothing was \
             deleted. {}.",
            capitalise(&lines[0].2)
        ))
        .await;
    }

    Ok(ReconcileOutcome {
        interrupted,
        missing: report.missing_worktrees,
    })
}

/// The open tasks that have a worktree, by project. A folder task has none, so it never appears.
pub(crate) fn tasks_with_worktrees(open: &[TaskRow]) -> BTreeMap<ProjectId, BTreeSet<TaskId>> {
    let mut by_project: BTreeMap<ProjectId, BTreeSet<TaskId>> = BTreeMap::new();
    for t in open
        .iter()
        .filter(|t| has_worktree(t.status) && t.delivery != DeliveryMode::Folder)
    {
        by_project.entry(t.project).or_default().insert(t.id);
    }
    by_project
}

/// Git's view of the task worktrees against the open tasks. A task still in phase C may not
/// have its worktree yet: its runner makes it (or reuses what the interrupted attempt left), so
/// it isn't reported missing. A closed task's branch is kept by design (closing keeps the branch,
/// for you to merge or delete), so it isn't an orphan.
async fn worktree_report(svc: &TaskService) -> Result<ReconcileReport, StartupError> {
    let (projects, preparing, closed) = svc
        .db()
        .call(|conn| {
            let projects = active_projects(conn)?;
            let open = task_store::open(conn)?;
            let mut by_project = tasks_with_worktrees(&open);
            let preparing: BTreeSet<TaskId> = open
                .iter()
                .filter(|t| t.status == TaskStatus::Preparing)
                .map(|t| t.id)
                .collect();
            // A folder project has no worktrees, and git never runs in it.
            let projects = projects
                .into_iter()
                .filter(|p| p.delivery != DeliveryMode::Folder)
                .map(|p| ReconcileProject {
                    project: p.id,
                    checkout: p.path,
                    tasks_with_worktrees: by_project.remove(&p.id).unwrap_or_default(),
                })
                .collect::<Vec<_>>();
            let closed: BTreeSet<TaskId> = task_store::with_status(conn, TaskStatus::Closed)?
                .into_iter()
                .map(|t| t.id)
                .collect();
            Ok((projects, preparing, closed))
        })
        .await?;
    let git = svc.deps().git.clone();
    let tasks_dir = svc.deps().data_dir.tasks_dir();
    let mut report =
        tokio::task::spawn_blocking(move || git_reconcile(&git, &tasks_dir, &projects))
            .await
            .map_err(|e| StartupError::Git(e.to_string()))?
            .map_err(|e| StartupError::Git(e.to_string()))?;
    report
        .missing_worktrees
        .retain(|m| !preparing.contains(&m.task));
    report.orphan_branches.retain(|o| !closed.contains(&o.task));

    Ok(report)
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One event per finding (kept refs alone are information).
fn findings(report: &ReconcileReport) -> Vec<(&'static str, Option<TaskId>, String)> {
    let mut out = Vec::new();
    for u in &report.unexpected_worktrees {
        out.push((
            "reconcile.unexpected_worktree",
            u.task,
            format!("{} is a worktree no open task owns", u.path.display()),
        ));
    }
    for m in &report.missing_worktrees {
        out.push((
            "reconcile.missing_worktree",
            Some(m.task),
            format!(
                "{}'s worktree is missing ({})",
                m.task,
                m.expected_path.display()
            ),
        ));
    }
    for s in &report.stale_registrations {
        out.push((
            "reconcile.stale_registration",
            None,
            format!(
                "git still lists a worktree at {} that is gone",
                s.path.display()
            ),
        ));
    }
    for o in &report.orphan_branches {
        out.push((
            "reconcile.orphan_branch",
            Some(o.task),
            format!(
                "branch {} at {} belongs to no open task",
                o.branch,
                o.commit.short()
            ),
        ));
    }
    for p in &report.unreadable_projects {
        out.push((
            "reconcile.unreadable_project",
            None,
            format!("{} can't be read: {}", p.checkout.display(), p.reason),
        ));
    }
    out
}

/// The time of day a restart note names, in this Mac's time zone.
fn hh_mm(at: Timestamp) -> String {
    at.to_zoned(TimeZone::system())
        .strftime("%H:%M")
        .to_string()
}

/// Step 6 (after the seat opens): every open task, oldest first.
#[allow(clippy::too_many_lines)]
pub async fn catch_up(svc: &TaskService, outcome: ReconcileOutcome) {
    let open = match svc.db().call(|conn| task_store::open(conn)).await {
        Ok(open) => open,
        Err(e) => {
            tracing::error!(error = %e, "couldn't read the open tasks at startup");
            return;
        }
    };
    let missing: BTreeMap<TaskId, MissingWorktree> =
        outcome.missing.into_iter().map(|m| (m.task, m)).collect();
    let mut reroute = false;
    for row in open {
        let task = row.id;
        match row.status {
            TaskStatus::Failed | TaskStatus::Stopped | TaskStatus::Discarded => {
                let svc = svc.clone();
                tokio::spawn(async move {
                    let exit = Exit::Stopped { reason: None };
                    if let Err(e) = retire(&svc, task, exit).await {
                        tracing::warn!(%task, error = %e, "an interrupted retirement didn't finish");
                    }
                });
                continue;
            }
            TaskStatus::Queued => {
                reroute = true;
                continue;
            }
            _ => {}
        }
        let agent = match svc
            .db()
            .call(move |conn| agents::crewmate_of(conn, task))
            .await
        {
            Ok(Some(a)) => a.id,
            _ => continue,
        };
        if let Some(m) = missing.get(&task)
            && !reattach(svc, task, m).await
        {
            not_resumed(svc, task, &row.branch, m.branch_exists).await;
            continue;
        }
        if row.status == TaskStatus::Preparing {
            svc.spawn_runner(task, agent, RunnerStart::Prepare);
            continue;
        }
        let cut = svc
            .db()
            .call(move |conn| cut_short(conn, agent))
            .await
            .unwrap_or_default();
        if let Some(last) = cut.last() {
            let turn = last.turn;
            let at = svc.clock().now();
            let text = format!(
                "The app restarted at {}; your last turn was cut short. Carry on from where you were.",
                hh_mm(at)
            );
            let turns: Vec<_> = cut.iter().map(|c| c.turn).collect();
            let result = svc
                .write(move |tx, notify| {
                    let wake = new_wake(
                        agent,
                        WakeKind::Restarted,
                        task,
                        format!("restarted:{task}:{turn}"),
                        &text,
                        "the app restarted",
                        serde_json::json!({}),
                        at,
                    );
                    Inbox::post(tx, &wake, notify)?;
                    for t in &turns {
                        mark_recovered(tx, *t)?;
                    }
                    task_store::event(
                        tx,
                        RESTARTED_EVENT,
                        task,
                        Some(agent),
                        &format!("{task} was resumed after the restart"),
                        serde_json::json!({ "turn": turn }),
                        at,
                    )
                    .map(drop)
                })
                .await;
            if let Err(e) = result {
                tracing::warn!(%task, error = %e, "couldn't post the restart note");
            }
            svc.spawn_runner(task, agent, RunnerStart::Resume);
            continue;
        }
        // Idle: the idle ladder comes back for the same spell, when one was running.
        let spell = if row.status == TaskStatus::Running {
            svc.db()
                .call(move |conn| agents::last_finished_turn(conn, agent))
                .await
                .ok()
                .flatten()
                .and_then(|(turn, status, reason)| {
                    let by_you = status == TurnStatus::Interrupted
                        && reason.as_deref() == Some("interrupted");
                    (!by_you).then_some((turn, reason.as_deref() == Some("provider")))
                })
        } else {
            None
        };
        // A spell that ended in a clear report started no ladder: the task would be reported.
        svc.spawn_runner(task, agent, RunnerStart::Idle { idle_spell: spell });
    }
    if reroute {
        svc.schedule_reroute();
    }
}

/// Puts a vanished worktree back on its existing branch, with the project's listed
/// files copied again. The setup command isn't rerun when it already finished.
async fn reattach(svc: &TaskService, task: TaskId, missing: &MissingWorktree) -> bool {
    if !missing.branch_exists {
        return false;
    }
    let project_id = missing.project;
    let Ok(Some(project)) = svc
        .db()
        .call(move |conn| load_project(conn, project_id))
        .await
    else {
        return false;
    };
    let git = svc.deps().git.clone();
    let paths = svc.deps().data_dir.task(task);
    let files = project.files_to_copy.clone();
    let result = tokio::task::spawn_blocking(move || {
        let repo = qm_git::Repo::open(git, &project.path)?;
        let attached = repo.reattach_worktree(task, &paths)?;
        repo.copy_files_into(&attached.path, &files)?;
        Ok::<_, qm_git::GitError>(attached)
    })
    .await;
    match result {
        Ok(Ok(attached)) => {
            let at = svc.clock().now();
            let summary = format!(
                "{task}'s missing worktree was put back on {} at {}",
                attached.branch,
                attached.head.short()
            );
            let _ = svc
                .write(move |tx, _| {
                    task_store::event(
                        tx,
                        RESTARTED_EVENT,
                        task,
                        None,
                        &summary,
                        serde_json::json!({}),
                        at,
                    )
                    .map(drop)
                })
                .await;
            true
        }
        Ok(Err(e)) => {
            tracing::warn!(%task, error = %e, "couldn't reattach the missing worktree");
            false
        }
        Err(e) => {
            tracing::warn!(%task, error = %e, "couldn't reattach the missing worktree");
            false
        }
    }
}

/// A task whose worktree is missing isn't resumed: an event, and a wake for its supervisor.
async fn not_resumed(svc: &TaskService, task: TaskId, branch: &str, branch_exists: bool) {
    let at = svc.clock().now();
    let text = if branch_exists {
        format!(
            "{task}'s worktree is missing (its branch {branch} is safe), so it wasn't resumed. stop_task keeps its work in a kept ref."
        )
    } else {
        format!(
            "{task}'s worktree and its branch {branch} are missing, so it wasn't resumed. stop_task closes it."
        )
    };
    let result = svc
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(());
            };
            task_store::event(
                tx,
                RESTARTED_EVENT,
                task,
                None,
                &format!("{task} wasn't resumed: its worktree is missing"),
                serde_json::json!({}),
                at,
            )?;
            let wake = new_wake(
                row.supervisor,
                WakeKind::Watchdog,
                task,
                format!("restarted:{task}:missing_worktree"),
                &text,
                &format!("{task} not resumed"),
                serde_json::json!({}),
                at,
            );
            Inbox::post(tx, &wake, notify).map(drop)
        })
        .await;
    if let Err(e) = result {
        tracing::warn!(%task, error = %e, "couldn't report a missing worktree");
    }
}
