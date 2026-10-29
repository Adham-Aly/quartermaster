//! Startup and resume.
//!
//! [`reconcile`] runs before the seat opens (step 4) and deletes nothing: turns still running, of
//! every role, become interrupted with their wakes released (the seat's and secondmates' loops
//! then recover theirs as cut short), live crewmates are marked stopped, task worktrees are
//! matched against tasks, and anything that needs a look is one event each and one note in the
//! Bridge. [`catch_up`] runs once the seat is open (step 6): side exits finish their
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
use qm_git::Git;
use qm_git::{MissingWorktree, ReconcileProject, ReconcileReport, reconcile as git_reconcile};
use qm_store::StoreError;
use qm_store::agents::{self, InterruptedTurn};
use qm_store::events::{self, NewEvent};
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::turns::{cut_short, mark_recovered};
use qm_types::data_dir::{DataDir, remove_path};
use qm_types::domain::{DeliveryMode, TaskStatus, TurnStatus, WakeKind};
use qm_types::ids::{PortBlock, ProjectId, SecondmateId, TaskId};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
    pub interrupted: Vec<InterruptedTurn>,
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

/// What a start deletes from the data directory and the scratch root (plan §9): the Quartermaster's
/// and each secondmate's session folder, the folders, evidence and scratchpads of tasks no longer open
/// (an open task keeps its folder but loses its session and its legacy answer file), and the
/// folders of retired secondmates. Worktrees, repositories, the workspace and Claude's own files
/// are left alone. A file that can't be removed is logged, not fatal.
async fn remove_leftover_files(svc: &TaskService) -> Result<(), StartupError> {
    let (open, live) = svc
        .db()
        .call(|conn| {
            let open = qm_store::context::open_tasks(conn)?
                .into_iter()
                .map(|t| t.task)
                .collect::<Vec<_>>();
            let live = qm_store::secondmates::active(conn)?
                .into_iter()
                .map(|s| s.id)
                .collect::<Vec<_>>();
            Ok((open, live))
        })
        .await?;
    let data = svc.deps().data_dir.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || sweep_leftovers(&data, &open, &live)).await
    {
        tracing::warn!(error = %e, "sweeping leftover files stopped");
    }
    crate::retention::sweep(
        svc.db(),
        &svc.deps().data_dir,
        svc.clock(),
        crate::retention::When::Startup,
    )
    .await;
    Ok(())
}

/// The blocking half of [`remove_leftover_files`]; `open` are the open tasks, `live` the active
/// secondmates.
fn sweep_leftovers(data: &DataDir, open: &[TaskId], live: &[SecondmateId]) {
    if let Err(e) = qm_types::data_dir::private_folder(data.root()) {
        tracing::error!(error = %e, "unsafe data directory; no startup file cleanup performed");
        return;
    }
    let remove = |path: &Path| {
        if let Err(e) = remove_path(path) {
            tracing::warn!(path = %path.display(), error = %e, "couldn't remove a leftover file");
        }
    };
    remove(&data.seat().session);
    for (name, path) in subfolders(&data.secondmates_dir()) {
        match name.parse::<SecondmateId>() {
            Ok(id) if live.contains(&id) => {
                let current = data.secondmate(id);
                // A folder under the older name (`SM-02`) moves to the current one (`S-02`).
                if path != current.root {
                    if current.root.exists() {
                        remove(&path);
                    } else if let Err(e) = std::fs::rename(&path, &current.root) {
                        tracing::warn!(path = %path.display(), error = %e, "couldn't rename a secondmate's folder");
                    }
                }
                remove(&current.session);
            }
            Ok(_) => remove(&path),
            Err(_) => {}
        }
    }
    // A crewmate's own folder: `tasks/C-0042`, or `tasks/T-0042` from an older version.
    for (name, root) in subfolders(&data.tasks_dir()) {
        let Ok(id) = name.parse::<TaskId>() else {
            continue;
        };
        // Plans, evidence and transcripts are the retention sweep's (`crate::retention`), which
        // runs next and moves an open crewmate's old evidence folder in.
        if open.contains(&id) {
            remove(&root.join("session"));
            remove(&root.join("answer.md"));
        } else {
            remove(&root.join(qm_types::data_dir::EVIDENCE));
            remove(&root.join(qm_types::data_dir::TRANSCRIPT));
            if !root.join(qm_types::data_dir::WORKTREE).exists() {
                remove(&root);
            }
        }
    }
}

/// The names and paths of the folders directly in `dir`; none if it can't be read.
fn subfolders(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect()
}

/// The task numbers that task branches in `checkouts` name (local and remote-tracking) and that
/// folders in `tasks_dir` name. An earlier run left them behind, and the store can't see them, so
/// a fresh number must not take one. Blocking. An `Err` means a repository couldn't be read, so
/// the numbers it names are unknown (see [`reserve_scan`]).
pub(crate) fn leftover_task_numbers(
    git: &Git,
    tasks_dir: &Path,
    checkouts: &[PathBuf],
) -> Result<Vec<u32>, String> {
    let mut numbers = branch_task_numbers(git, checkouts)?;
    if let Ok(entries) = std::fs::read_dir(tasks_dir) {
        numbers.extend(
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<TaskId>().ok())
                .map(TaskId::get),
        );
    }
    numbers.sort_unstable();
    numbers.dedup();
    Ok(numbers)
}

/// The task numbers that task branches in each of `checkouts` name. Blocking. A checkout that
/// can't be read fails the whole scan: a number it names would otherwise look free.
pub(crate) fn branch_task_numbers(git: &Git, checkouts: &[PathBuf]) -> Result<Vec<u32>, String> {
    let mut numbers = Vec::new();
    for checkout in checkouts {
        let repo = qm_git::Repo::open(git.clone(), checkout)
            .map_err(|e| format!("{} couldn't be opened: {e}", checkout.display()))?;
        let found = repo.legacy_task_numbers().map_err(|e| {
            format!(
                "{} task branches couldn't be listed: {e}",
                checkout.display()
            )
        })?;
        numbers.extend(found.into_iter().map(TaskId::get));
    }
    Ok(numbers)
}

/// Reserves the numbers of a scan that worked, or holds task numbers when it didn't, so no task
/// takes a number that a branch might already name. Runs on the database thread.
pub(crate) fn reserve_scan(scan: Result<Vec<u32>, String>) {
    match scan {
        Ok(numbers) => qm_store::ids::reserve("tasks", numbers),
        Err(reason) => {
            tracing::error!(%reason, "task creation is held until the app restarts");
            qm_store::ids::hold("tasks", reason);
        }
    }
}

/// Reserves the task numbers git and the `tasks/` folder already name (see
/// [`leftover_task_numbers`]) on the database thread, where the store numbers tasks.
async fn reserve_leftover_task_numbers(svc: &TaskService) -> Result<(), StartupError> {
    let checkouts = svc
        .db()
        .call(|conn| {
            // A folder project has no task branches, and its folder may not be a repository.
            Ok(active_projects(conn)?
                .into_iter()
                .filter(|p| p.delivery != DeliveryMode::Folder)
                .map(|p| p.path)
                .collect::<Vec<_>>())
        })
        .await?;
    let (git, tasks_dir) = (svc.deps().git.clone(), svc.deps().data_dir.tasks_dir());
    let leftovers =
        tokio::task::spawn_blocking(move || leftover_task_numbers(&git, &tasks_dir, &checkouts))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
    svc.db()
        .call(move |_| {
            reserve_scan(leftovers);
            Ok(())
        })
        .await?;
    Ok(())
}

/// Scan after best-effort cleanup: even a pruned retired row can still have a folder, and that
/// folder must never become a new secondmate's home. Include every named entry, not just folders.
fn leftover_secondmate_numbers(dir: &Path) -> Result<Vec<u32>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{} couldn't be read: {e}", dir.display())),
    };
    let mut numbers = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{} couldn't be read: {e}", dir.display()))?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<SecondmateId>().ok())
        {
            numbers.push(id.get());
        }
    }
    Ok(numbers)
}

fn reserve_secondmate_scan(scan: Result<Vec<u32>, String>) {
    match scan {
        Ok(numbers) => qm_store::ids::reserve("secondmates", numbers),
        Err(reason) => {
            tracing::error!(%reason, "secondmate creation is held until the app restarts");
            qm_store::ids::hold("secondmates", reason);
        }
    }
}

async fn reserve_leftover_secondmate_numbers(svc: &TaskService) -> Result<(), StartupError> {
    let dir = svc.deps().data_dir.secondmates_dir();
    let scan = tokio::task::spawn_blocking(move || leftover_secondmate_numbers(&dir))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    svc.db()
        .call(move |_| {
            reserve_secondmate_scan(scan);
            Ok(())
        })
        .await?;
    Ok(())
}

/// Retires projects no open task works in, and sets each secondmate's projects from its open
/// crewmates: what a close would have done, had the app not stopped first.
async fn tidy_projects(svc: &TaskService) -> Result<(), StartupError> {
    let at = svc.clock().now();
    svc.write(move |tx, _| {
        for project in qm_store::projects::active(tx)? {
            crate::places::retire_if_idle(tx, project.id, at)?;
        }
        for sm in qm_store::secondmates::active(tx)? {
            qm_store::secondmates::sync_projects(tx, sm.id)?;
        }
        Ok(())
    })
    .await?;
    Ok(())
}

/// Step 4 (before the seat opens). Deletes only the leftover files [`remove_leftover_files`] names.
pub async fn reconcile(svc: &TaskService) -> Result<ReconcileOutcome, StartupError> {
    let at = svc.clock().now();
    let interrupted = svc
        .db()
        .call(move |conn| {
            let interrupted = agents::interrupt_running_turns(conn, at)?;
            agents::stop_live_crewmates(conn)?;
            Ok(interrupted)
        })
        .await?;

    // Before anything can be numbered: a new task never takes a number an earlier run left behind.
    reserve_leftover_task_numbers(svc).await?;
    remove_leftover_files(svc).await?;
    reserve_leftover_secondmate_numbers(svc).await?;

    tidy_projects(svc).await?;

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

/// The open tasks that have a worktree, with their recorded branches, by project. A folder task
/// has none, so it never appears.
pub(crate) fn tasks_with_worktrees(
    open: &[TaskRow],
) -> BTreeMap<ProjectId, BTreeMap<TaskId, String>> {
    let mut by_project: BTreeMap<ProjectId, BTreeMap<TaskId, String>> = BTreeMap::new();
    for t in open
        .iter()
        .filter(|t| has_worktree(t.status) && t.delivery != DeliveryMode::Folder)
    {
        by_project
            .entry(t.project)
            .or_default()
            .insert(t.id, t.branch.clone());
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

/// One event per finding (kept refs deleted at startup are information).
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
    svc.start_deferred_starts();
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
    let Ok(paths) = svc.paths(task).await else {
        return false;
    };
    let files = project.files_to_copy.clone();
    let branch = missing.branch.clone();
    let result = tokio::task::spawn_blocking(move || {
        let repo = qm_git::Repo::open(git, &project.path)?;
        let attached = repo.reattach_worktree(&branch, &paths)?;
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
            "{task}'s worktree is missing (its branch {branch} is safe), so it wasn't resumed. stop_task retires it."
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
