//! A secondmate's scope as code and the requests it may serve.
//!
//! "The code refuses tool calls on other projects, and the words guide judgment within them." The
//! list is the code's part: [`check_caller_project`] wraps `qm_policy::scope::check_project` for
//! every caller working under a secondmate (the secondmate itself, or one of its crewmates), and
//! is a no-op for the Quartermaster and its own crewmates. A secondmate whose list is empty works
//! on any registered project.

use qm_policy::scope::{ProjectScope, check_project};
use qm_store::StoreError;
use qm_store::secondmates::{self, RequestRow};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{RequestStatus, Role};
use qm_types::error::Refusal;
use qm_types::ids::{CorrelationId, ProjectId, SecondmateId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// A secondmate's project list, owned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedScope {
    /// Empty: unrestricted (words only).
    pub projects: Vec<ProjectId>,
}

impl OwnedScope {
    pub fn as_policy(&self) -> ProjectScope<'_> {
        if self.projects.is_empty() {
            ProjectScope::Unrestricted
        } else {
            ProjectScope::Listed(&self.projects)
        }
    }

    /// Whether `project` is inside the scope.
    pub fn contains(&self, project: ProjectId) -> bool {
        self.projects.is_empty() || self.projects.contains(&project)
    }
}

/// The secondmate's project list as it stands now (removed projects left out).
pub fn scope_of(conn: &Connection, secondmate: SecondmateId) -> Result<OwnedScope, StoreError> {
    Ok(OwnedScope {
        projects: secondmates::projects_of(conn, secondmate)?
            .into_iter()
            .map(|(id, _)| id)
            .collect(),
    })
}

/// The secondmate whose scope binds `caller`, if any: itself, or the one its crewmate works
/// under. The Quartermaster and its own crewmates are bound by none.
pub fn bound_by(caller: &Caller) -> Option<SecondmateId> {
    match caller.role {
        Role::Quartermaster => None,
        Role::Secondmate | Role::Crewmate => caller.secondmate,
    }
}

/// Refuses a call on `project` outside the caller's secondmate's scope, inside a database call.
pub fn check_project_in(
    conn: &Connection,
    caller: &Caller,
    project: ProjectId,
    project_name: &str,
) -> Result<Result<(), Refusal>, StoreError> {
    let Some(secondmate) = bound_by(caller) else {
        return Ok(Ok(()));
    };
    let scope = scope_of(conn, secondmate)?;
    Ok(check_project(scope.as_policy(), project, project_name))
}

/// [`check_project_in`] as a tool failure.
pub fn check_caller_project(
    conn: &Connection,
    caller: &Caller,
    project: ProjectId,
    project_name: &str,
) -> Result<(), ToolFailure> {
    match check_project_in(conn, caller, project, project_name) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(r)) => Err(r.into()),
        Err(e) => Err(ToolFailure::Failed(e.to_string())),
    }
}

/// Whether a caller bound by a secondmate may read `task`: its own task, or any task in its scope's
/// projects. Unbound callers read everything (seeing is allowed).
pub fn may_read_task(
    conn: &Connection,
    caller: &Caller,
    task: TaskId,
) -> Result<Result<(), Refusal>, StoreError> {
    let Some(secondmate) = bound_by(caller) else {
        return Ok(Ok(()));
    };
    let row: Option<(i64, ProjectId)> = conn
        .query_row(
            "SELECT supervisor_id, project_id FROM tasks WHERE id = ?1",
            params![task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((supervisor, project)) = row else {
        // "doesn't exist" is the lookup's own answer.
        return Ok(Ok(()));
    };
    let own = secondmates::agent_of(conn, secondmate)?.is_some_and(|a| a.get() == supervisor);
    if own || caller.agent.get() == supervisor || scope_of(conn, secondmate)?.contains(project) {
        Ok(Ok(()))
    } else {
        Ok(Err(Refusal::new(format!(
            "{task} is outside this secondmate's scope"
        ))))
    }
}

/// The request a secondmate's crewmate serves, checked: it exists, it is the caller's, and it is
/// not closed (an answered request may get more work, which reopens it).
pub fn request_for(
    conn: &Connection,
    secondmate: SecondmateId,
    correlation: CorrelationId,
) -> Result<Result<RequestRow, Refusal>, StoreError> {
    let Some(row) = secondmates::load_request(conn, correlation)? else {
        return Ok(Err(Refusal::new(format!(
            "There's no request {correlation}"
        ))));
    };
    if row.secondmate != secondmate {
        return Ok(Err(Refusal::new(format!(
            "{correlation} isn't one of your requests"
        ))));
    }
    if row.status == RequestStatus::Closed {
        return Ok(Err(Refusal::new(format!(
            "{correlation} is closed; ask the Quartermaster for a new request"
        ))));
    }
    Ok(Ok(row))
}

/// A routed request's overrides on a secondmate's start request: when the secondmate typed none of
/// its own, the request's model, effort, premium and later steps apply, with the routing turn's
/// authority (they were checked when the request was stored). Returns that authority when it
/// applied them. Nothing happens for other callers, a start without a correlation, or a correlation
/// that isn't the caller's (validation refuses those).
pub fn apply_request_overrides(
    conn: &Connection,
    caller: &Caller,
    start: &mut qm_tools::handler::StartRequest,
) -> Result<Option<qm_policy::overrides::Authority>, StoreError> {
    use qm_policy::overrides::Authority;
    use qm_types::domain::StartedBy;
    let (Role::Secondmate, Some(secondmate), Some(correlation)) =
        (caller.role, caller.secondmate, start.correlation)
    else {
        return Ok(None);
    };
    let Ok(row) = request_for(conn, secondmate, correlation)? else {
        return Ok(None);
    };
    let typed_any = start.model.is_some()
        || start.effort.is_some()
        || start.premium
        || !start.later_steps.is_empty();
    if typed_any || row.overrides.is_empty() {
        return Ok(None);
    }
    start.model.clone_from(&row.overrides.model);
    start.effort = row.overrides.effort;
    start.premium = row.overrides.premium;
    start.later_steps = row
        .overrides
        .later_steps
        .iter()
        .map(|s| qm_tools::defs::LaterStep {
            kind: s.kind,
            model: s.model.clone(),
            effort: s.effort,
        })
        .collect();
    let started_by = match secondmates::turn_message(conn, row.turn)? {
        Some(message) => StartedBy::User { message },
        None => StartedBy::AppEvent,
    };
    Ok(Some(Authority::Turn(started_by)))
}
