//! A secondmate's scope as code and the requests it may serve.
//!
//! A secondmate's projects are those of its open crewmates, kept in step by the app. They refuse
//! nothing: a secondmate starts work in any folder. They decide only what it may read: its own
//! tasks, and the tasks in its crewmates' projects.

use qm_store::StoreError;
use qm_store::secondmates::{self, RequestRow};
use qm_tools::handler::Caller;
use qm_types::domain::{RequestStatus, Role};
use qm_types::error::Refusal;
use qm_types::ids::{CorrelationId, ProjectId, SecondmateId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// A secondmate's project list, owned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedScope {
    pub projects: Vec<ProjectId>,
}

impl OwnedScope {
    /// Whether `project` is one of the secondmate's crewmates' projects.
    pub fn contains(&self, project: ProjectId) -> bool {
        self.projects.contains(&project)
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

/// A routed request's overrides on a secondmate's start request, field by field: a field the
/// secondmate typed wins, and every field it left empty (model, effort, premium, each `later_steps`
/// kind) comes from the request, with the routing turn's authority (they were checked when the
/// request was stored; what the secondmate typed was checked against its own turn). Returns that
/// authority when anything came from the request. A model the secondmate typed that isn't premium
/// is refused when the request asked for premium, rather than quietly running lower. Nothing
/// happens for other callers, a start without a correlation, or a correlation that isn't the
/// caller's (validation refuses those).
pub fn apply_request_overrides(
    conn: &Connection,
    caller: &Caller,
    start: &mut qm_tools::handler::StartRequest,
) -> Result<Result<Option<qm_policy::overrides::Authority>, Refusal>, StoreError> {
    use qm_policy::overrides::Authority;
    use qm_types::domain::{StartedBy, Tier};
    let (Role::Secondmate, Some(secondmate), Some(correlation)) =
        (caller.role, caller.secondmate, start.correlation)
    else {
        return Ok(Ok(None));
    };
    let Ok(row) = request_for(conn, secondmate, correlation)? else {
        return Ok(Ok(None));
    };
    let asked = &row.overrides;
    if asked.is_empty() {
        return Ok(Ok(None));
    }
    let request_premium = asked.premium
        || match &asked.model {
            Some(m) => crate::routing::tiers_of(conn, m)?.contains(&Tier::Premium),
            None => false,
        };
    if request_premium
        && let Some(typed) = &start.model
        && !crate::routing::tiers_of(conn, typed)?.contains(&Tier::Premium)
    {
        return Ok(Err(Refusal::new(format!(
            "{typed} isn't in the premium level and the request asked for premium; leave the \
             model out or name a premium model"
        ))));
    }
    let mut applied = false;
    if start.model.is_none() && asked.model.is_some() {
        start.model.clone_from(&asked.model);
        applied = true;
    }
    if start.effort.is_none() && asked.effort.is_some() {
        start.effort = asked.effort;
        applied = true;
    }
    if !start.premium && asked.premium {
        start.premium = true;
        applied = true;
    }
    for step in &asked.later_steps {
        if start.later_steps.iter().any(|s| s.kind == step.kind) {
            continue;
        }
        start.later_steps.push(qm_tools::defs::LaterStep {
            kind: step.kind,
            model: step.model.clone(),
            effort: step.effort,
            premium: step.premium,
        });
        applied = true;
    }
    if !applied {
        return Ok(Ok(None));
    }
    let started_by = match secondmates::turn_message(conn, row.turn)? {
        Some(message) => StartedBy::User { message },
        None => StartedBy::AppEvent,
    };
    Ok(Ok(Some(Authority::Turn(started_by))))
}
