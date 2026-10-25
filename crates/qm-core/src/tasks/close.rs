//! Closing and retiring: one path for every exit.
//!
//! 1. **Record the exit** (one transaction): `failed`, `stopped` or `discarded`; a landed or
//!    answered task keeps its status until step 4. `task.closing`, and for a failure the
//!    supervisor's `task_failed` wake.
//! 2. **Stop the crewmate**: its runner interrupts a running turn, closes the session and stops
//!    every job or setup group still open for the task.
//! 3. **Retire the worktree**: the worktree removed (uncommitted changes with it) and the branch
//!    deleted, except a delivered branch-only task's branch. Unmerged commits are left for git to
//!    collect. A folder task has no worktree or branch: its files stay where they are, and nothing
//!    is undone.
//! 4. **Close** (one transaction): `closed` with its outcome, the crewmate retired (it leaves the
//!    crew), `task.closed`, and `wait_over` for every task waiting on it. Then the app's own files
//!    for it go: its folder and scratchpad at once, its plan, evidence and report once nothing
//!    needs them ([`crate::retention`]).
//!
//! Every step is idempotent: running `retire` again finishes an interrupted retirement (at
//! startup too), and every status change is a compare-and-swap. A git failure leaves the task in
//! its exit status, reported; nothing is removed, and the next launch tries again.

use super::lifecycle::{TaskEvent, is_exiting, next_for, outcome_for};
use super::runner::stop_open_groups;
use super::waits::{check_direct, load_task, release_waiters};
use super::{TaskService, load_project, new_wake, refuse};
use crate::inbox::Inbox;
use qm_git::{BranchRemoval, Repo, RetireSpec};
use qm_policy::delivery::close_keeps_branch;
use qm_store::agents;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::TaskUpdated;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{AgentStatus, DeliveryMode, TaskStatus, WakeKind};
use qm_types::ids::TaskId;

/// `events.kind` of an exit being recorded.
pub const CLOSING_EVENT: &str = "task.closing";
/// `events.kind` of a closed task.
pub const CLOSED_EVENT: &str = "task.closed";
/// `events.kind` of a retirement that failed.
pub const CLOSE_FAILED_EVENT: &str = "task.close_failed";
/// `events.kind` of your Continue on a stuck notice.
pub const CONTINUED_EVENT: &str = "task.continued";

/// Delivery's hook on every retirement.
pub trait CloseFacts: Send + Sync + std::fmt::Debug {
    /// After a side exit (failed, stopped, discarded) is recorded: an open pull request is closed.
    /// Runs in the background; never blocks the retirement.
    fn on_side_exit(&self, task: TaskId) -> qm_types::BoxFuture<'_, ()>;
}

impl TaskService {
    /// Sets delivery's close hook (once; a second call is ignored).
    pub fn set_close_facts(&self, hook: std::sync::Arc<dyn CloseFacts>) {
        if self.inner.close_facts.set(hook).is_err() {
            tracing::warn!("the close hook was set twice; the first one stays");
        }
    }

    fn close_hook(&self) -> Option<std::sync::Arc<dyn CloseFacts>> {
        self.inner.close_facts.get().cloned()
    }
}

/// How a task ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    Landed,
    Answered,
    Failed { reason: String },
    Stopped { reason: Option<String> },
    Discarded,
}

impl Exit {
    fn status(&self) -> Option<TaskStatus> {
        match self {
            Self::Failed { .. } => Some(TaskStatus::Failed),
            Self::Stopped { .. } => Some(TaskStatus::Stopped),
            Self::Discarded => Some(TaskStatus::Discarded),
            Self::Landed | Self::Answered => None,
        }
    }

    fn event(&self) -> TaskEvent {
        match self {
            Self::Failed { .. } => TaskEvent::Failed,
            Self::Stopped { .. } => TaskEvent::Stopped,
            Self::Discarded => TaskEvent::Discarded,
            Self::Landed | Self::Answered => TaskEvent::Closed,
        }
    }

    /// The exit a task already in a side-exit status is finishing.
    fn of_status(status: TaskStatus) -> Option<Self> {
        match status {
            TaskStatus::Failed => Some(Self::Failed {
                reason: String::new(),
            }),
            TaskStatus::Stopped => Some(Self::Stopped { reason: None }),
            TaskStatus::Discarded => Some(Self::Discarded),
            _ => None,
        }
    }
}

/// Whether `row`'s newest done report delivered an answer (read failures count as not).
async fn answered(svc: &TaskService, row: &TaskRow) -> bool {
    let row = row.clone();
    svc.db()
        .call(move |conn| super::delivered::of_task(conn, &row))
        .await
        .is_ok_and(|d| d == qm_policy::delivery::Deliverable::Answer)
}

/// What retiring did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Retired {
    pub branch: BranchRemoval,
}

/// Why a retirement didn't finish.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CloseError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Failed(String),
}

impl From<CloseError> for ToolFailure {
    fn from(e: CloseError) -> Self {
        match e {
            CloseError::Refused(r) => refuse(r),
            CloseError::Failed(m) => ToolFailure::Failed(m),
        }
    }
}

/// Retires `task` by `exit`: every exit the same way.
#[allow(clippy::too_many_lines)]
pub(crate) async fn retire(
    svc: &TaskService,
    task: TaskId,
    exit: Exit,
) -> Result<Retired, CloseError> {
    let Some(_guard) = svc.begin_retiring(task) else {
        return Err(CloseError::Refused(format!(
            "{task} is already being closed"
        )));
    };
    let row = svc
        .db()
        .call(move |conn| task_store::load(conn, task))
        .await
        .map_err(|e| CloseError::Failed(e.to_string()))?
        .ok_or_else(|| CloseError::Refused(format!("{task} doesn't exist")))?;
    if row.status == TaskStatus::Closed {
        return Ok(Retired {
            branch: BranchRemoval::AlreadyGone,
        });
    }

    // 1. Record the exit.
    let exit = match (exit.status(), is_exiting(row.status)) {
        // Already ending: finish that retirement.
        (_, true) => Exit::of_status(row.status).unwrap_or(exit),
        (Some(_), false) => {
            let to = next_for(task, row.status, exit.event())
                .map_err(|r| CloseError::Refused(r.reason))?;
            let at = svc.clock().now();
            let from = row.status;
            let supervisor = row.supervisor;
            let recorded = exit.clone();
            svc.write(move |tx, notify| {
                task_store::set_status(tx, task, from, to, at)?;
                let (summary, reason) = match &recorded {
                    Exit::Failed { reason } => {
                        (format!("{task} failed: {reason}"), Some(reason.clone()))
                    }
                    Exit::Stopped { reason } => {
                        (format!("{task} is being stopped"), reason.clone())
                    }
                    Exit::Discarded => (format!("{task} is being discarded"), None),
                    Exit::Landed | Exit::Answered => (format!("{task} is closing"), None),
                };
                task_store::event(
                    tx,
                    CLOSING_EVENT,
                    task,
                    None,
                    &summary,
                    serde_json::json!({ "exit": to, "reason": reason }),
                    at,
                )?;
                if let Exit::Failed { reason } = &recorded {
                    let mut text = format!("{task} failed before it could finish: {reason}.");
                    if row.delivery == DeliveryMode::Folder {
                        text.push_str(" Its files in the folder are left as they are.");
                    }
                    let wake = new_wake(
                        supervisor,
                        WakeKind::TaskFailed,
                        task,
                        format!("failed:{task}"),
                        &text,
                        &format!("{task} failed"),
                        serde_json::json!({}),
                        at,
                    );
                    Inbox::post(tx, &wake, notify)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| CloseError::Failed(e.to_string()))?;
            if let Some(hook) = svc.close_hook() {
                tokio::spawn(async move { hook.on_side_exit(task).await });
            }
            exit
        }
        (None, false) => {
            let ok = match exit {
                Exit::Landed => row.status == TaskStatus::Landed,
                Exit::Answered => row.status == TaskStatus::Reported && answered(svc, &row).await,
                _ => false,
            };
            if !ok {
                return Err(CloseError::Refused(format!(
                    "{task} is {}; it can't close that way",
                    row.status
                )));
            }
            exit
        }
    };

    // 2. Stop the crewmate (qm-git expects it stopped first).
    if svc.stop_runner(task).await.is_none() {
        let agent = svc
            .db()
            .call(move |conn| agents::crewmate_of(conn, task))
            .await
            .ok()
            .flatten()
            .map(|a| a.id);
        stop_open_groups(svc, task, agent).await;
        if let Some(agent) = agent {
            let at = svc.clock().now();
            let _ = svc
                .write(move |tx, _| {
                    agents::set_status(tx, agent, AgentStatus::Stopped, at)?;
                    qm_store::jobs::close_running(tx, agent, at).map(drop)
                })
                .await;
        }
    }

    // 3. Retire the worktree. A folder task has none, so nothing on disk is touched.
    let project = svc
        .db()
        .call(move |conn| load_project(conn, row.project))
        .await
        .map_err(|e| CloseError::Failed(e.to_string()))?;
    let folder_left = match (&project, row.delivery) {
        (Some(project), DeliveryMode::Folder) => Some(project.path.display().to_string()),
        _ => None,
    };
    let retired = match project {
        Some(project) if row.status != TaskStatus::Queued && folder_left.is_none() => {
            let git = svc.deps().git.clone();
            let paths = svc.paths_of(&row);
            // Only a task whose base was recorded made its branch (creation records the base before
            // it makes the branch), so only it may delete the branch: any other is kept as it is.
            let keep_branch = row.base_commit.is_none()
                || (matches!(exit, Exit::Landed) && close_keeps_branch(row.delivery));
            let checkout = project.path.clone();
            let branch = row.branch.clone();
            let result = tokio::task::spawn_blocking(move || {
                let repo = Repo::open(git, &checkout)?;
                repo.retire(&RetireSpec::new(&branch, &paths).keep_branch(keep_branch))
            })
            .await
            .map_err(|e| CloseError::Failed(e.to_string()))?;
            match result {
                Ok(r) => Retired { branch: r.branch },
                Err(e) => {
                    let reason = e.to_string();
                    let at = svc.clock().now();
                    let summary = format!("{task} couldn't be retired: {reason}");
                    let event_summary = summary.clone();
                    let _ = svc
                        .write(move |tx, _| {
                            task_store::event(
                                tx,
                                CLOSE_FAILED_EVENT,
                                task,
                                None,
                                &event_summary,
                                serde_json::json!({ "reason": reason }),
                                at,
                            )
                            .map(drop)
                        })
                        .await;
                    svc.app_note(&format!(
                        "{summary}. Nothing was removed; the app tries again at its next launch."
                    ))
                    .await;
                    return Err(CloseError::Failed(summary));
                }
            }
        }
        // Never prepared, or its project is gone: nothing on disk to retire.
        _ => Retired {
            branch: BranchRemoval::AlreadyGone,
        },
    };

    // 4. Close.
    let at = svc.clock().now();
    svc.write(move |tx, notify| {
        let Some(now) = task_store::load(tx, task)? else {
            return Ok(());
        };
        if now.status == TaskStatus::Closed {
            return Ok(());
        }
        let outcome = outcome_for(now.status, super::delivered::of_task(tx, &now)?);
        task_store::set_closed(tx, task, now.status, outcome, at)?;
        // Nothing may be approved for a task that no longer exists. Superseding never approves
        // or rejects; it only takes the card (and its inbox item) away.
        let pending: Vec<qm_types::ids::HeldActionId> = tx
            .prepare("SELECT id FROM held_actions WHERE status = 'pending' AND task_id = ?1")?
            .query_map(rusqlite::params![task], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for held in pending {
            crate::held::HeldService::supersede_with(tx, held, Some(task), "the task closed", at)?;
        }
        if let Some(agent) = agents::crewmate_of(tx, task)? {
            agents::set_status(tx, agent.id, AgentStatus::Retired, at)?;
            let sm: Option<qm_types::ids::SecondmateId> = tx.query_row(
                "SELECT secondmate_id FROM agents WHERE id = ?1",
                rusqlite::params![agent.id],
                |r| r.get(0),
            )?;
            if let Some(sm) = sm {
                qm_store::secondmates::sync_projects(tx, sm)?;
            }
        }
        // A project lives while work runs in it: its last open task's close retires it.
        crate::places::retire_if_idle(tx, now.project, at)?;
        let summary = match &folder_left {
            Some(folder) => {
                format!("{task} closed: {outcome} · its files in {folder} are left as they are")
            }
            None => format!("{task} closed: {outcome}"),
        };
        task_store::event(
            tx,
            CLOSED_EVENT,
            task,
            None,
            &summary,
            serde_json::json!({ "outcome": outcome }),
            at,
        )?;
        release_waiters(tx, notify, task, TaskStatus::Closed, at)
    })
    .await
    .map_err(|e| CloseError::Failed(e.to_string()))?;
    // The task's folder (its transcript too) and scratchpad go with the close: its outcome and
    // answer are in the store. Its plan and evidence go once no open task reads them, and its
    // report once you have read it (the retention sweep, below). Only the app's own files go:
    // nothing a crewmate wrote elsewhere is touched. A failure is logged; the startup sweep
    // removes what it leaves.
    let paths = svc.paths_of(&row);
    let removed = tokio::task::spawn_blocking(move || {
        qm_types::data_dir::remove_path(&paths.root)?;
        qm_types::data_dir::remove_path(&paths.scratchpad)
    })
    .await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(%task, error = %e, "couldn't remove a closed task's files"),
        Err(e) => tracing::warn!(%task, error = %e, "removing a closed task's files stopped"),
    }
    crate::retention::sweep(
        svc.db(),
        &svc.deps().data_dir,
        svc.clock(),
        crate::retention::When::Running,
    )
    .await;
    // Closed tasks beyond the newest few hundred leave memory (Q8). A failure here doesn't undo the
    // close; the next close tries again.
    let forgot = svc
        .db()
        .call(|conn| {
            qm_store::history::forget_closed_tasks(conn, qm_store::history::KEEP_CLOSED_TASKS)
        })
        .await;
    match forgot {
        Ok(gone) if !gone.is_empty() => {
            tracing::info!(count = gone.len(), "forgot the oldest closed tasks");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%task, error = %e, "couldn't forget the oldest closed tasks"),
    }
    Ok(retired)
}

impl TaskService {
    /// Fails a task with its reason and retires it (phase C, a lost session).
    pub(crate) async fn fail(&self, task: TaskId, reason: String) {
        if let Err(e) = retire(self, task, Exit::Failed { reason }).await {
            tracing::warn!(%task, error = %e, "a failed task couldn't be retired");
        }
    }

    /// Stops a task and retires it (a decision card's "Drop task" calls this too).
    pub async fn stop(&self, task: TaskId, reason: Option<String>) -> Result<(), String> {
        retire(self, task, Exit::Stopped { reason })
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Discards a task and retires it.
    pub async fn discard(&self, task: TaskId) -> Result<(), String> {
        retire(self, task, Exit::Discarded)
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Your Continue on a stuck notice (the decision performer): `stuck → running`, and the
    /// crewmate is told, which starts a new idle spell after its next turn.
    pub async fn continue_after_stuck(&self, task: TaskId) -> Result<(), String> {
        let at = self.clock().now();
        self.write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(Err(format!("{task} doesn't exist")));
            };
            let to = match next_for(task, row.status, TaskEvent::Continued) {
                Ok(to) => to,
                Err(r) => return Ok(Err(r.reason)),
            };
            task_store::set_status(tx, task, row.status, to, at)?;
            let event = task_store::event(
                tx,
                CONTINUED_EVENT,
                task,
                None,
                &format!("you chose Continue on {task}"),
                serde_json::json!({}),
                at,
            )?;
            if let Some(agent) = agents::crewmate_of(tx, task)? {
                let wake = new_wake(
                    agent.id,
                    WakeKind::Watchdog,
                    task,
                    format!("continue:{task}:{event}"),
                    "The user chose Continue. Carry on with your task, and report when you're done.",
                    "carry on",
                    serde_json::json!({}),
                    at,
                );
                Inbox::post(tx, &wake, notify)?;
            }
            Ok(Ok(()))
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// `stop_task`.
pub(crate) async fn stop_task(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
    reason: Option<String>,
) -> Result<TaskUpdated, ToolFailure> {
    let row = load_task(svc, task).await?;
    check_direct(svc, caller, &row).await?;
    if row.status == TaskStatus::Closed {
        return Ok(TaskUpdated {
            task,
            status: TaskStatus::Closed,
        });
    }
    svc.supervisor_acted(task);
    retire(svc, task, Exit::Stopped { reason }).await?;
    Ok(TaskUpdated {
        task,
        status: TaskStatus::Closed,
    })
}

/// `close_task`.
pub(crate) async fn close_task(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
) -> Result<TaskUpdated, ToolFailure> {
    let row = load_task(svc, task).await?;
    check_direct(svc, caller, &row).await?;
    let exit = match row.status {
        TaskStatus::Closed => {
            return Ok(TaskUpdated {
                task,
                status: TaskStatus::Closed,
            });
        }
        TaskStatus::Landed => Exit::Landed,
        TaskStatus::Reported if answered(svc, &row).await => Exit::Answered,
        TaskStatus::Reported => {
            return Err(refuse(format!(
                "{task} has work that isn't settled: settle_task delivers it, stop_task drops it"
            )));
        }
        s if is_exiting(s) => Exit::of_status(s).unwrap_or(Exit::Stopped { reason: None }),
        s => {
            return Err(refuse(format!(
                "{task} is still {s}; stop it with stop_task"
            )));
        }
    };
    svc.supervisor_acted(task);
    retire(svc, task, exit).await.map_err(ToolFailure::from)?;
    Ok(TaskUpdated {
        task,
        status: TaskStatus::Closed,
    })
}
