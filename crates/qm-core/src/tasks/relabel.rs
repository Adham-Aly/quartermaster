//! Moving work between labels: a crewmate's project and its task.
//!
//! `set_task_project`: an orchestrator changes the project label of one of its in-place
//! crewmates. The label restricts nothing; it says where the work belongs. A worktree's label
//! follows its repository, so only in-place work moves. The old project is retired if no open
//! work is left in it, and the supervising secondmate's projects follow.
//!
//! `set_task_label`: an orchestrator moves one of its crewmates to another task (`T-`), or to a
//! new one of its own; the crewmate's name (`C-`) never changes. The store records the new task at
//! once, and the crewmate gets a `task_changed` wake naming its new folders, which starts its next
//! turn. Its artifacts and scratchpad (`artifacts/T-t/C-n`, `scratchpad/T-t/C-n`) follow in
//! [`follow_task`]: at the start of that turn, before its session reopens and unless a background
//! job is writing to them, or at once when it has no runner. A link is left at each old path.
//!
//! A task given where a crewmate is asked for is read as that task's crewmate
//! ([`resolve_task_label`]).

use super::start::{find_place, join_refusal};
use super::waits::{check_direct, load_task};
use super::{TaskService, failed, refuse};
use crate::places;
use crate::verification::wake_crewmate;
use qm_store::tasks as task_store;
use qm_store::{projects, task_labels};
use qm_tools::defs::{TaskChanged, TaskUpdated};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{TaskStatus, WakeKind};
use qm_types::ids::{SecondmateId, TaskId, TaskLabelId};
use rusqlite::OptionalExtension;
use std::path::Path;
use std::{fs, io};

/// The event recorded when a task's project label changes.
pub const RELABELLED_EVENT: &str = "task.relabelled";
/// The event recorded when a crewmate moves to another task.
pub const TASK_CHANGED_EVENT: &str = "crewmate.task_changed";

/// `set_task_project`.
pub(crate) async fn set_task_project(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
    project: Option<String>,
    folder: Option<String>,
) -> Result<TaskUpdated, ToolFailure> {
    let row = load_task(svc, task).await?;
    check_direct(svc, caller, &row).await?;
    if row.status == TaskStatus::Closed {
        return Err(refuse(format!(
            "{task} is closed; its label stays as it was"
        )));
    }
    if !row.in_place {
        let old = row.project;
        let name = svc
            .db()
            .call(move |conn| Ok(projects::load(conn, old)?.map(|p| p.name)))
            .await
            .map_err(|e| failed(&e))?
            .unwrap_or_else(|| "its project".to_owned());
        return Err(refuse(format!(
            "{task} works in a worktree of {name}; its label follows its repository"
        )));
    }
    let place = find_place(svc, project, folder, false).await?;
    let home = svc.deps().home.clone();
    let at = svc.clock().now();
    let status = row.status;
    let old = row.project;
    svc.write(move |tx, _| {
        let new = places::register(tx, &place, &home, at)?;
        if !task_store::set_project(tx, task, new.id)? {
            return Ok(());
        }
        let from = projects::load(tx, old)?.map_or_else(String::new, |p| p.name);
        task_store::event(
            tx,
            RELABELLED_EVENT,
            task,
            None,
            &format!("{task} moved from {from} to {}", new.name),
            serde_json::json!({ "from": old, "to": new.id }),
            at,
        )?;
        let sm: Option<SecondmateId> = tx
            .query_row(
                "SELECT secondmate_id FROM agents WHERE task_id = ?1 AND role = 'crewmate'",
                rusqlite::params![task],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(sm) = sm {
            qm_store::secondmates::sync_projects(tx, sm)?;
        }
        places::retire_if_idle(tx, old, at)?;
        Ok(())
    })
    .await
    .map_err(|e| failed(&e))?;
    Ok(TaskUpdated { task, status })
}

/// `set_task_label`.
pub(crate) async fn set_task_label(
    svc: &TaskService,
    caller: &Caller,
    crewmate: TaskId,
    task: Option<TaskLabelId>,
) -> Result<TaskChanged, ToolFailure> {
    let row = load_task(svc, crewmate).await?;
    check_direct(svc, caller, &row).await?;
    if row.status == TaskStatus::Closed {
        return Err(refuse(format!(
            "{crewmate} is closed; it stays in {}",
            row.label
        )));
    }
    let was = row.label;
    let caller = *caller;
    let at = svc.clock().now();
    let data = svc.deps().data_dir.clone();
    let outcome = svc
        .write(move |tx, notify| {
            let label = match task {
                Some(label) => {
                    if let Some(why) = join_refusal(tx, &caller, label, "task")? {
                        return Ok(Err(why));
                    }
                    label
                }
                // Already alone in its task: a new one would only change the number.
                None if task_labels::crewmates(tx, was)?
                    .iter()
                    .all(|(c, _)| *c == crewmate) =>
                {
                    was
                }
                None => task_labels::create(tx, Some(crewmate.get()), at)?,
            };
            let with = others_in(tx, label, crewmate)?;
            if task_store::set_label(tx, crewmate, label)? {
                task_store::event(
                    tx,
                    TASK_CHANGED_EVENT,
                    crewmate,
                    None,
                    &format!("{crewmate} moved from {was} to {label}"),
                    serde_json::json!({ "from": was.to_string(), "to": label.to_string() }),
                    at,
                )?;
                let paths = data.task(crewmate, label);
                let others = if with.is_empty() {
                    String::new()
                } else {
                    let names: Vec<String> = with.iter().map(ToString::to_string).collect();
                    format!(", with {}", names.join(", "))
                };
                let text = format!(
                    "Your supervisor moved you to task {label}{others}. Your evidence folder is now {} and your scratchpad {}; the old paths lead there too.",
                    paths.evidence.display(),
                    paths.scratchpad.display()
                );
                wake_crewmate(
                    tx,
                    notify,
                    crewmate,
                    WakeKind::TaskChanged,
                    format!("task_changed:{label}:{}", at.as_millisecond()),
                    &text,
                    &format!("moved to {label}"),
                    at,
                )?;
            }
            Ok(Ok((label, with)))
        })
        .await
        .map_err(|e| failed(&e))?;
    let (task, with) = outcome.map_err(ToolFailure::Refused)?;
    if task != was
        && !svc.has_runner(crewmate)
        && let Err(e) = follow_task(svc, crewmate).await
    {
        // Kept for the next turn, or the sweep once the crewmate closes.
        tracing::warn!(%crewmate, error = %e, "a crewmate's folders couldn't follow it to its new task yet");
    }
    svc.changed();
    Ok(TaskChanged {
        crewmate,
        task,
        was,
        with,
    })
}

/// The open crewmates in `label` other than `crewmate`.
fn others_in(
    conn: &rusqlite::Connection,
    label: TaskLabelId,
    crewmate: TaskId,
) -> Result<Vec<TaskId>, qm_store::StoreError> {
    Ok(task_labels::crewmates(conn, label)?
        .into_iter()
        .filter(|(c, open)| *open && *c != crewmate)
        .map(|(c, _)| c)
        .collect())
}

/// Moves `crewmate`'s artifacts and scratchpad under its current task when they are still under
/// an earlier one, leaving a link at each old path. `Ok(false)` when there was nothing to move. Call only while no session of the crewmate's is
/// open: a provider session's write roots are fixed when it opens.
pub(crate) async fn follow_task(svc: &TaskService, crewmate: TaskId) -> Result<bool, String> {
    let row = svc
        .db()
        .call(move |conn| task_store::load(conn, crewmate))
        .await
        .map_err(|e| e.to_string())?;
    let Some(row) = row.filter(|r| r.label != r.folder_label) else {
        return Ok(false);
    };
    let data = &svc.deps().data_dir;
    let from = data.task(crewmate, row.folder_label);
    let to = data.task(crewmate, row.label);
    tokio::task::spawn_blocking(move || {
        move_folder(&from.artifacts, &to.artifacts)?;
        move_folder(&from.scratchpad, &to.scratchpad)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let label = row.label;
    svc.write(move |tx, _| task_store::set_folder_label(tx, crewmate, label))
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Moves one of a crewmate's folders, leaving a link at the old path.
fn move_folder(from: &Path, to: &Path) -> io::Result<()> {
    // A link already there was left by an earlier move: the crewmate is moving back.
    if fs::symlink_metadata(to).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(to)?;
    }
    qm_types::data_dir::migrate_directory(from, to)
}

/// The crewmate `task` means where a crewmate is asked for: its one open crewmate, else its only
/// one. A task that no longer exists means the crewmate numbered like it, when there is one: the
/// name every crewmate had before tasks held crewmates.
pub(crate) async fn resolve_task_label(
    svc: &TaskService,
    task: TaskLabelId,
) -> Result<TaskId, ToolFailure> {
    let legacy = task.legacy_crewmate();
    let (exists, crew, numbered) = svc
        .db()
        .call(move |conn| {
            let exists = task_labels::exists(conn, task)?;
            let crew = task_labels::crewmates(conn, task)?;
            let numbered = task_store::load(conn, legacy)?.is_some();
            Ok((exists, crew, numbered))
        })
        .await
        .map_err(|e| failed(&e))?;
    let open: Vec<TaskId> = crew.iter().filter(|(_, o)| *o).map(|(c, _)| *c).collect();
    let all: Vec<TaskId> = crew.iter().map(|(c, _)| *c).collect();
    match (open.as_slice(), all.as_slice()) {
        ([one], _) | ([], [one]) => Ok(*one),
        ([], []) if !exists && numbered => Ok(legacy),
        ([], []) => Err(refuse(format!(
            "{task} holds no crewmates; name a crewmate (C-n) from the context block or find_tasks"
        ))),
        (open, all) => {
            let named = if open.is_empty() { all } else { open };
            let names: Vec<String> = named.iter().map(ToString::to_string).collect();
            Err(refuse(format!(
                "{task} holds {}; name the crewmate you mean",
                names.join(", ")
            )))
        }
    }
}
