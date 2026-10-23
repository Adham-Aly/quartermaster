//! `set_task_project`: an orchestrator changes the project label of one of its in-place tasks.
//!
//! The label restricts nothing; it says where the work belongs. A worktree task's label follows its
//! repository, so only in-place tasks move. The old project is retired if no open task is left in
//! it, and the supervising secondmate's projects follow.

use super::start::find_place;
use super::waits::{check_direct, load_task};
use super::{TaskService, failed, refuse};
use crate::places;
use qm_store::projects;
use qm_store::tasks as task_store;
use qm_tools::defs::TaskUpdated;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::TaskStatus;
use qm_types::ids::{SecondmateId, TaskId};
use rusqlite::OptionalExtension;

/// The event recorded when a task's project label changes.
pub const RELABELLED_EVENT: &str = "task.relabelled";

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
    let place = find_place(svc, project, folder).await?;
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
