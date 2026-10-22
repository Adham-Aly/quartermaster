//! Correlation IDs: a routed request's identity from the routing call to its answer.
//!
//! A `CorrelationId` (`C-0193`) **is** the request's `secondmate_requests` row ID, allocated when
//! the request is routed. It travels: `route_to_secondmate`'s result → the routed wake's
//! `corr="C-0193"` → `start_crewmate.correlation` → `tasks.secondmate_request_id` → every fact row
//! on the parent channel → `parent_report.correlation` → the Quartermaster's wake and event.
//!
//! The request's status: `open` when routed → `answered` when a report arrives while none of its
//! tasks is open → back to `open` when another of its tasks starts → `closed` only when its
//! secondmate retires (secondmates don't retire yet). Each change is a compare-and-swap in the
//! transaction of the row that causes it.

use crate::tasks::lifecycle::is_open;
use qm_store::StoreError;
use qm_store::parent_channel::{self as store, RequestRecord};
use qm_tools::handler::ToolFailure;
use qm_types::domain::{RequestStatus, TaskStatus};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, CorrelationId, SecondmateId, TaskId};
use rusqlite::{Connection, params};

/// A request, as the parent channel needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestFacts {
    pub id: CorrelationId,
    pub secondmate: SecondmateId,
    /// The secondmate's agent.
    pub agent: AgentId,
    /// The secondmate's supervisor (the Quartermaster's agent): whom the channel wakes.
    pub parent: AgentId,
    pub scope: String,
    pub request: String,
    pub status: RequestStatus,
}

impl From<RequestRecord> for RequestFacts {
    fn from(r: RequestRecord) -> Self {
        Self {
            id: r.id,
            secondmate: r.secondmate,
            agent: r.agent,
            parent: r.parent,
            scope: r.scope,
            request: r.request,
            status: r.status,
        }
    }
}

/// A request by its correlation ID.
pub fn request(conn: &Connection, corr: CorrelationId) -> Result<Option<RequestFacts>, StoreError> {
    Ok(store::request(conn, corr)?.map(RequestFacts::from))
}

/// `parent_report`'s correlation: one of `secondmate`'s own requests, not closed (an answered one
/// stays reportable). Refusals: "There's no request C-0190", "C-0190 isn't one of your requests",
/// "C-0190 is closed".
pub fn check_report_correlation(
    conn: &Connection,
    secondmate: SecondmateId,
    corr: CorrelationId,
) -> Result<RequestFacts, ToolFailure> {
    let Some(req) = request(conn, corr).map_err(|e| ToolFailure::Failed(e.to_string()))? else {
        return Err(Refusal::new(format!("There's no request {corr}")).into());
    };
    if req.secondmate != secondmate {
        return Err(Refusal::new(format!("{corr} isn't one of your requests")).into());
    }
    if req.status == RequestStatus::Closed {
        return Err(Refusal::new(format!("{corr} is closed")).into());
    }
    Ok(req)
}

/// The tasks started for a request, oldest first.
pub fn tasks_of(conn: &Connection, corr: CorrelationId) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT id FROM tasks WHERE secondmate_request_id = ?1 ORDER BY seq")?;
    let rows = stmt
        .query_map(params![corr], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn any_open(conn: &Connection, corr: CorrelationId) -> Result<bool, StoreError> {
    let mut stmt = conn.prepare("SELECT status FROM tasks WHERE secondmate_request_id = ?1")?;
    let statuses = stmt
        .query_map(params![corr], |r| r.get::<_, TaskStatus>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(statuses.into_iter().any(is_open))
}

/// What happened to a request, for the status rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusCause {
    /// The secondmate reported on it.
    Report,
    /// A task for it started.
    TaskStarted,
    /// Any other fact on one of its tasks.
    Fact,
}

/// The status rule, in the caller's transaction. Returns the status after it.
pub fn apply_status_rule(
    conn: &Connection,
    corr: CorrelationId,
    current: RequestStatus,
    cause: StatusCause,
) -> Result<RequestStatus, StoreError> {
    let to = match (current, cause) {
        (RequestStatus::Open, StatusCause::Report) if !any_open(conn, corr)? => {
            RequestStatus::Answered
        }
        (RequestStatus::Answered, StatusCause::TaskStarted) => RequestStatus::Open,
        _ => return Ok(current),
    };
    Ok(if store::set_request_status(conn, corr, current, to)? {
        to
    } else {
        current
    })
}
