//! Durable starts without a child task, branch, runner, or port allocation before the trigger.
//!
//! One service-owned pulse observes facts. The ordinary start path revalidates the request,
//! and atomically consumes the schedule in its task-record transaction. Cancellation wins
//! even while a firing start is settling its base. Forgotten/reused references fail closed.
use crate::inbox::Inbox;
use crate::tasks::{
    TaskService, failed, refuse,
    start::{self, StartAs},
};
use jiff::Timestamp;
use qm_policy::overrides::{Authority, OverrideKind, check_choice_override};
use qm_store::{
    StoreError,
    deferred::{self, DeferredRow},
};
use qm_tools::defs::{Ack, DeferredStart, StartTrigger};
use qm_tools::handler::{Caller, StartRequest, ToolFailure, TurnCx};
use qm_types::domain::{ReportOutcome, Role, StartedBy, TaskOutcome, TaskStatus, WakeKind};
use qm_types::ids::{AgentId, MessageId, SecondmateId, TaskId, TurnId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    request: StartRequest,
    /// Typed fields were authorized separately, before filling delegated fields.
    #[serde(default)]
    typed_request: Option<StartRequest>,
    #[serde(default)]
    delegated: Option<DelegatedSource>,
    #[serde(default)]
    base: Option<FrozenBase>,
    role: Role,
    secondmate: Option<SecondmateId>,
    turn: Option<TurnId>,
    user_message: Option<MessageId>,
    words: String,
    words_message: Option<MessageId>,
    /// Every task reference, including `builds_on/retry`, is an identity, not just a number.
    pins: Vec<(TaskId, i64)>,
    request_seq: Option<i64>,
    #[serde(default)]
    started_task_seq: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DelegatedSource {
    parent: AgentId,
    parent_seq: i64,
    message: MessageId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FrozenBase {
    branch: Option<String>,
}
impl FrozenBase {
    fn request(&self) -> qm_git::BaseRequest {
        self.branch
            .as_ref()
            .map_or(qm_git::BaseRequest::Default, |branch| {
                qm_git::BaseRequest::Typed {
                    branch: branch.clone(),
                    traced_to_user: true,
                }
            })
    }
}

impl Saved {
    fn authority(&self) -> Authority {
        Authority::Turn(
            self.user_message
                .map_or(StartedBy::AppEvent, |message| StartedBy::User { message }),
        )
    }
    fn caller(&self, row: &DeferredRow) -> Caller {
        Caller {
            agent: row.owner,
            role: self.role,
            secondmate: self.secondmate,
            turn: self.turn.map(|turn| TurnCx {
                turn,
                started_by: self
                    .user_message
                    .map_or(StartedBy::AppEvent, |message| StartedBy::User { message }),
            }),
            task: None,
            plan: None,
        }
    }
}
fn decode<T: serde::de::DeserializeOwned>(
    column: &'static str,
    text: &str,
) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(|source| StoreError::Json { column, source })
}
fn encode<T: Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|source| StoreError::Json {
        column: "deferred_starts.payload",
        source,
    })
}
fn view(row: DeferredRow) -> Result<DeferredStart, StoreError> {
    Ok(DeferredStart {
        schedule: row.id,
        trigger: decode("deferred_starts.trigger_json", &row.trigger)?,
        status: row.status,
        task: row.task,
        note: row.note,
    })
}
fn target(trigger: &StartTrigger) -> Option<TaskId> {
    match trigger {
        StartTrigger::Commit { task }
        | StartTrigger::Done { task }
        | StartTrigger::Landed { task } => Some(*task),
        StartTrigger::At { .. } => None,
    }
}
fn orchestrator(caller: &Caller) -> Result<(), ToolFailure> {
    if matches!(caller.role, Role::Quartermaster | Role::Secondmate) {
        Ok(())
    } else {
        Err(refuse("only an orchestrator may schedule crewmate starts"))
    }
}
fn check_overrides(
    request: &StartRequest,
    authority: Authority,
    role: Role,
) -> Result<(), qm_types::error::Refusal> {
    for (present, kind) in [
        (request.model.is_some(), OverrideKind::NamedModel),
        (request.tier.is_some(), OverrideKind::Tier),
        (request.effort.is_some(), OverrideKind::Effort),
        (request.premium, OverrideKind::Premium),
        (!request.later_steps.is_empty(), OverrideKind::LaterSteps),
        (
            request.base_branch.is_some() && role != Role::Secondmate,
            OverrideKind::BaseBranch,
        ),
        (request.in_place, OverrideKind::InPlace),
        (
            request.verification.is_some(),
            OverrideKind::VerificationLevel,
        ),
        (
            request.stronger_retry_of.is_some(),
            OverrideKind::StrongerModelRetry,
        ),
    ] {
        if present {
            check_choice_override(authority, request.choice_message, kind)?;
        }
    }
    Ok(())
}
fn pin(conn: &Connection, task: TaskId) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .query_row("SELECT seq FROM tasks WHERE id = ?1", [task], |r| r.get(0))
        .optional()?)
}

impl TaskService {
    /// Registers intent only. Time triggers already due are allowed and fire on the next pulse.
    pub async fn schedule_crewmate(
        &self,
        caller: &Caller,
        mut request: StartRequest,
        trigger: StartTrigger,
    ) -> Result<DeferredStart, ToolFailure> {
        orchestrator(caller)?;
        if self.is_shutting_down() {
            return Err(refuse("the app is quitting; schedule after it restarts"));
        }
        qm_tools::handlers::crew::check_start_shape(&request)?;
        let caller = *caller;
        let at = self.clock().now();
        self.write(move |tx, _| {
            let user_message = caller.turn.and_then(|t| match t.started_by { StartedBy::User { message } => Some(message), StartedBy::AppEvent => None });
            if request.model.is_some() || request.tier.is_some() || request.effort.is_some() || request.premium || request.preference.is_some() {
                request.choice_message = request.choice_message.or(user_message);
                if let Some(message) = request.choice_message
                    && let Err(r) = qm_store::user_message_sources::verify(tx, caller.agent, message)? { return Ok(Err(r.into())); }
            }
            let authority = Authority::Turn(user_message.map_or(StartedBy::AppEvent, |message| StartedBy::User { message }));
            if let Err(r) = check_overrides(&request, authority, caller.role) { return Ok(Err(r.into())); }
            let owner: Option<(i64, Role, Option<SecondmateId>)> = tx.query_row(
                "SELECT seq, role, secondmate_id FROM agents WHERE id = ?1 AND retired_at IS NULL", [caller.agent],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((owner_seq, role, secondmate)) = owner else { return Ok(Err(refuse("the scheduling owner is no longer active"))); };
            if role != caller.role || secondmate != caller.secondmate { return Ok(Err(refuse("the scheduling caller's identity changed"))); }
            let mut pins = Vec::new();
            for task in target(&trigger).into_iter().chain(request.builds_on).chain(request.stronger_retry_of) {
                if let Err(r) = crate::secondmates::scope::may_read_task(tx, &caller, task)? { return Ok(Err(r.into())); }
                let Some(seq) = pin(tx, task)? else { return Ok(Err(refuse(format!("{task} is unavailable; choose a kept task")))); };
                if !pins.iter().any(|(t, _)| *t == task) { pins.push((task, seq)); }
            }
            let request_seq = if let Some(correlation) = request.correlation {
                if let Some(sm) = caller.secondmate
                    && let Err(r) = crate::secondmates::scope::request_for(tx, sm, correlation)? { return Ok(Err(r.into())); }
                tx.query_row("SELECT seq FROM secondmate_requests WHERE id = ?1", [correlation], |r| r.get(0)).optional()?
            } else { None };
            if request.correlation.is_some() && request_seq.is_none() { return Ok(Err(refuse("the routed request is unavailable"))); }
            // Freeze the user's words now, before chat compaction or engine-owned chat drops them.
            let (words, words_message) = if let Some(message) = user_message {
                (qm_store::messages::text(tx, message)?.unwrap_or_default(), Some(message))
            } else {
                let mut inherited = None;
                for task in request.stronger_retry_of.into_iter().chain(request.builds_on) {
                    if let Some(row) = qm_store::tasks::load(tx, task)?
                        && (request.stronger_retry_of == Some(task) || row.supervisor == caller.agent) {
                        inherited = Some((row.user_words, row.user_words_message)); break;
                    }
                }
                if let Some(words) = inherited { words } else if let Some(c) = request.correlation {
                    let words = tx.query_row("SELECT user_words FROM secondmate_requests WHERE id = ?1", [c], |r| r.get(0)).optional()?.unwrap_or_default();
                    (words, None)
                } else { (String::new(), None) }
            };
            let typed_request = Some(request.clone());
            let inherited = match crate::secondmates::scope::apply_request_overrides(tx, &caller, &mut request)? {
                Ok(authority) => authority,
                Err(r) => return Ok(Err(r.into())),
            };
            let delegated = if inherited.is_some() {
                let Some(sm) = qm_store::secondmates::by_agent(tx, caller.agent)? else { return Ok(Err(refuse("the delegating secondmate is no longer active"))); };
                let message = request.choice_message.or(match inherited {
                    Some(Authority::Turn(StartedBy::User { message })) => Some(message),
                    _ => None,
                });
                if let Some(message) = message {
                    if qm_store::user_message_sources::verify(tx, caller.agent, message)?.is_ok() {
                        None
                    } else {
                        // Delegation is restricted to the request's actual source, not any
                        // other message the child happens to know from the parent's conversation.
                        let routed = qm_store::secondmates::load_request(tx, request.correlation.expect("checked delegation"))?.expect("checked request");
                        let source = routed.overrides.choice_message.or(qm_store::secondmates::turn_message(tx, routed.turn)?);
                        if source != Some(message) {
                            return Ok(Err(refuse("delegated model choices must use the routed request's original user-message source")));
                        }
                        if let Err(r) = qm_store::user_message_sources::verify(tx, sm.parent, message)? { return Ok(Err(r.into())); }
                        let parent_seq = tx.query_row("SELECT seq FROM agents WHERE id = ?1", [sm.parent], |r| r.get(0))?;
                        Some(DelegatedSource { parent: sm.parent, parent_seq, message })
                    }
                } else { None }
            } else { None };
            let base = if role == Role::Secondmate {
                match start::freeze_base(tx, &caller, &request, authority)? {
                    Ok(base) => Some(FrozenBase { branch: base.and_then(|b| match b {
                        qm_git::BaseRequest::Typed { branch, .. } => Some(branch),
                        qm_git::BaseRequest::Default => None,
                    }) }),
                    Err(r) => return Ok(Err(r.into())),
                }
            } else { None };
            let saved = Saved { request, typed_request, delegated, base, role, secondmate, turn: caller.turn.map(|t| t.turn), user_message, words, words_message, pins, request_seq, started_task_seq: None };
            let id = deferred::insert(tx, caller.agent, owner_seq, &encode(&trigger)?, &encode(&saved)?, at)?;
            Ok(Ok(view(deferred::get(tx, id)?.expect("just inserted"))?))
        }).await.map_err(|e| failed(&e))?
    }

    pub async fn list_crewmate_starts(
        &self,
        caller: &Caller,
    ) -> Result<Vec<DeferredStart>, ToolFailure> {
        orchestrator(caller)?;
        let owner = caller.agent;
        self.db()
            .call(move |conn| {
                deferred::list(conn, Some(owner), false)?
                    .into_iter()
                    .map(view)
                    .collect()
            })
            .await
            .map_err(|e| failed(&e))
    }
    pub async fn cancel_crewmate_start(
        &self,
        caller: &Caller,
        schedule: i64,
    ) -> Result<Ack, ToolFailure> {
        orchestrator(caller)?;
        let owner = caller.agent;
        let at = self.clock().now();
        let cancelled = self
            .write(move |tx, _| deferred::cancel(tx, schedule, owner, at))
            .await
            .map_err(|e| failed(&e))?;
        if !cancelled {
            return Err(refuse(
                "that is not one of your pending crewmate starts (it may have already started)",
            ));
        }
        Ok(Ack {
            note: "Cancelled; no crewmate will start from this schedule.".to_owned(),
        })
    }
}

/// Readiness is transient; invalid identity/scope is a terminal refusal.
pub(crate) enum RecordReadiness {
    Ready,
    Waiting,
}

/// Last gate, in the task-record transaction. Also used before starting settlement.
/// Returns a refusal on cancellation, retirement, forgotten/reused IDs, or changed scope.
pub(crate) fn check_record(
    conn: &Connection,
    id: i64,
    caller: &Caller,
    at: Timestamp,
) -> Result<Result<RecordReadiness, qm_types::error::Refusal>, StoreError> {
    use qm_types::error::Refusal;
    let Some(row) = deferred::get(conn, id)? else {
        return Ok(Err(Refusal::new("the deferred start is unavailable")));
    };
    if row.status != "pending" || row.owner != caller.agent {
        return Ok(Err(Refusal::new("the deferred start is no longer pending")));
    }
    let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
    let active: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM agents WHERE id = ?1 AND seq = ?2 AND retired_at IS NULL AND role = ?3 AND secondmate_id IS ?4)", params![row.owner, row.owner_seq, saved.role, saved.secondmate], |r| r.get(0))?;
    if !active {
        return Ok(Err(Refusal::new(
            "the scheduling owner retired or changed identity",
        )));
    }
    for (task, seq) in &saved.pins {
        if pin(conn, *task)? != Some(*seq) {
            return Ok(Err(Refusal::new(format!(
                "{task} was forgotten or replaced; this start cannot safely fire"
            ))));
        }
        if let Err(r) = crate::secondmates::scope::may_read_task(conn, caller, *task)? {
            return Ok(Err(r));
        }
    }
    if let Some(c) = saved.request.correlation {
        let seq: Option<i64> = conn
            .query_row(
                "SELECT seq FROM secondmate_requests WHERE id = ?1",
                [c],
                |r| r.get(0),
            )
            .optional()?;
        if seq != saved.request_seq {
            return Ok(Err(Refusal::new(
                "the routed request was forgotten or replaced",
            )));
        }
        if let Some(sm) = saved.secondmate
            && let Err(r) = crate::secondmates::scope::request_for(conn, sm, c)?
        {
            return Ok(Err(r));
        }
    }
    let typed = saved.typed_request.as_ref().unwrap_or(&saved.request);
    if let Some(message) = typed.choice_message
        && (typed.model.is_some()
            || typed.tier.is_some()
            || typed.effort.is_some()
            || typed.premium
            || typed.preference.is_some())
        && let Err(r) = qm_store::user_message_sources::verify(conn, row.owner, message)?
    {
        return Ok(Err(r));
    }
    if let Err(r) = check_overrides(typed, saved.authority(), saved.role) {
        return Ok(Err(r));
    }
    let trigger: StartTrigger = decode("deferred_starts.trigger_json", &row.trigger)?;
    // Commit is an observed historical fact (read outside SQLite); the other facts can
    // and must be checked again after asynchronous base settlement, before allocating a task.
    if !matches!(trigger, StartTrigger::Commit { .. }) && !ready_in_store(conn, &trigger, at)? {
        return Ok(Ok(RecordReadiness::Waiting));
    }
    Ok(Ok(RecordReadiness::Ready))
}

/// Consume intent and pin the newly allocated identity in the same task transaction.
pub(crate) fn record_started(
    conn: &Connection,
    id: i64,
    task: TaskId,
    at: Timestamp,
) -> Result<bool, StoreError> {
    if !deferred::mark_started(conn, id, task, at)? {
        return Ok(false);
    }
    let row = deferred::get(conn, id)?.expect("just consumed");
    let mut saved: Saved = decode("deferred_starts.payload", &row.payload)?;
    saved.started_task_seq = pin(conn, task)?;
    conn.execute(
        "UPDATE deferred_starts SET payload = ?2 WHERE id = ?1",
        params![id, encode(&saved)?],
    )?;
    Ok(true)
}

pub(crate) struct FrozenRequest {
    pub base: Option<qm_git::BaseRequest>,
}

/// Only an exact replay of a service-saved request may bypass typed-source ownership checks.
/// Scope/identity is still checked before and inside the task-record transaction.
pub(crate) fn frozen_request(
    conn: &Connection,
    id: i64,
    caller: &Caller,
    request: &StartRequest,
) -> Result<Option<FrozenRequest>, StoreError> {
    let Some(row) = deferred::get(conn, id)? else {
        return Ok(None);
    };
    let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
    if row.status != "pending"
        || row.owner != caller.agent
        || saved.role != caller.role
        || saved.secondmate != caller.secondmate
        || saved.typed_request.is_none()
        || saved.request != *request
    {
        return Ok(None);
    }
    Ok(Some(FrozenRequest {
        base: saved.base.as_ref().map(FrozenBase::request),
    }))
}

/// A saved delegation authorizes only the exact authority-bearing fields frozen for this child.
/// Preference is independent guidance; `frozen_request` still checks the entire replayed request.
/// The original source and its owning agent identity must still exist, even after chat retention.
pub(crate) fn verifies_delegated_choice(
    conn: &Connection,
    facts: &crate::routing::CrewmateFacts,
    message: MessageId,
) -> Result<bool, StoreError> {
    let mut stmt = conn.prepare("SELECT payload, owner_agent_id, owner_seq FROM deferred_starts WHERE status = 'started' AND task_id = ?1")?;
    let rows = stmt.query_map([facts.task], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, AgentId>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (payload, owner, owner_seq) = row?;
        let saved: Saved = decode("deferred_starts.payload", &payload)?;
        let Some(source) = saved.delegated else {
            continue;
        };
        if source.message != message
            || saved.request.choice_message != Some(message)
            || saved.request.model != facts.named_model
            || saved.request.tier != facts.named_tier
            || saved.request.effort != facts.named_effort
            || saved.request.premium != facts.premium
        {
            continue;
        }
        let active: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1 AND seq = ?2 AND role = 'secondmate' AND retired_at IS NULL AND secondmate_id IS ?3 AND supervisor_id = ?4)", params![owner, owner_seq, saved.secondmate, source.parent], |r| r.get(0))?;
        let parent: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1 AND seq = ?2)",
            params![source.parent, source.parent_seq],
            |r| r.get(0),
        )?;
        let task = qm_store::tasks::load(conn, facts.task)?;
        let task_seq = pin(conn, facts.task)?;
        if active
            && parent
            && saved.started_task_seq == task_seq
            && task.is_some_and(|t| {
                t.supervisor == owner
                    && t.secondmate_request == saved.request.correlation.map(|c| i64::from(c.get()))
            })
            && qm_store::user_message_sources::verify(conn, source.parent, message)?.is_ok()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ready_in_store(
    conn: &Connection,
    trigger: &StartTrigger,
    at: Timestamp,
) -> Result<bool, StoreError> {
    Ok(match trigger {
        StartTrigger::At { due } => at >= *due,
        StartTrigger::Done { task } => qm_store::reports::latest_clear(conn, *task)?
            .is_some_and(|r| r.outcome == Some(ReportOutcome::Done)),
        StartTrigger::Landed { task } => qm_store::tasks::load(conn, *task)?.is_some_and(|r| {
            r.status == TaskStatus::Landed
                || (r.status == TaskStatus::Closed && r.outcome == Some(TaskOutcome::Landed))
        }),
        StartTrigger::Commit { .. } => false,
    })
}

/// Observes an actual local, task-owned non-merge commit, without fetching or trusting
/// inherited HEAD. Exclude both the original base and the current target tips after a rebase.
pub(crate) async fn committed(
    svc: &TaskService,
    task: TaskId,
) -> Result<Option<qm_types::ids::CommitSha>, ToolFailure> {
    let (row, project) = svc
        .db()
        .call(move |conn| {
            let row = qm_store::tasks::load(conn, task)?;
            let project = match &row {
                Some(r) => qm_store::projects::load(conn, r.project)?,
                None => None,
            };
            Ok((row, project))
        })
        .await
        .map_err(|e| failed(&e))?;
    let (Some(row), Some(project)) = (row, project) else {
        return Err(refuse("the trigger task or its project was forgotten"));
    };
    let Some(base) = row.base_commit else {
        return Ok(None);
    };
    let worktree = svc.deps().data_dir.task(task).worktree;
    if row.status != TaskStatus::Closed && !worktree.exists() {
        return Ok(None);
    }
    let git = svc.deps().git.clone();
    tokio::task::spawn_blocking(move || -> Result<_, qm_git::GitError> {
        let repo = qm_git::Repo::open(git, &project.path)?;
        let head = if row.status == TaskStatus::Closed {
            // Closed worktrees are retired. Check the kept SHA in this repository, too.
            row.head_commit
        } else {
            let head = repo.worktree_head(&worktree)?;
            if repo.local_branch_commit(&row.branch)? != head {
                return Ok(None);
            }
            head
        };
        let Some(head) = head else {
            return Ok(None);
        };
        if head == base {
            return Ok(None);
        }
        if row.status == TaskStatus::Closed {
            // A kept report is a historical commit fact even after its work lands on the
            // target. Resolve both objects locally, but do not exclude the now-landed head.
            repo.changed_files(&base, &head)?;
            return Ok(Some(head));
        }
        let target = row
            .base_branch
            .as_deref()
            .unwrap_or(&project.default_branch);
        Ok(repo
            .has_own_local_commit(&head, &base, target)?
            .then_some(head))
    })
    .await
    .map_err(|e| failed(&e))?
    .map_err(|e| failed(&e))
}

/// A pulse never allocates a child while a condition is pending. Store failures leave intent
/// pending for the next pulse; a refused launch records its reason and wakes its owner.
pub async fn pulse(svc: &TaskService) -> Result<(), StoreError> {
    if svc.is_shutting_down() {
        return Ok(());
    }
    recover_started(svc).await?;
    let rows = svc
        .db()
        .call(|conn| deferred::list(conn, None, true))
        .await?;
    for row in rows {
        if svc.is_shutting_down() {
            break;
        }
        let result = fire(svc, &row).await;
        if let Err(error) = result {
            if svc.is_shutting_down() {
                break;
            }
            if let ToolFailure::Failed(reason) = &error {
                tracing::warn!(schedule = row.id, error = %reason, "deferred start infrastructure failed; retrying on next pulse");
                continue;
            }
            let fallback = svc.deps().seat_agent;
            let note = error.render();
            let id = row.id;
            let at = svc.clock().now();
            svc.write(move |tx, notify| {
                if deferred::fail(tx, id, &note, at)? {
                    let text = format!("Deferred crewmate start {id} failed: {note}. No child task was started.");
                    let owner_exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1 AND seq = ?2)", params![row.owner, row.owner_seq], |r| r.get(0))?;
                    let target = if owner_exists { row.owner } else { fallback };
                    Inbox::post(tx, &qm_store::wakes::NewWake { target, kind: WakeKind::ScheduleDue,
                        payload: serde_json::json!({ "text": text, "short": "Deferred crewmate start failed", "schedule": id }),
                        dedupe_key: format!("deferred-start:{id}:failed"), task: None, at }, notify)?;
                }
                Ok(())
            }).await?;
        }
    }
    Ok(())
}
/// A failed mirror flush can hide a committed start from its caller. Recover only launch
/// states, never restart a completed/failed task; `spawn_runner` itself deduplicates live runners.
async fn recover_started(svc: &TaskService) -> Result<(), StoreError> {
    let launches = svc.db().call(|conn| {
        let mut stmt = conn.prepare("SELECT t.id, a.id, t.status, d.id FROM deferred_starts d JOIN tasks t ON t.id = d.task_id JOIN agents a ON a.task_id = t.id AND a.role = 'crewmate' JOIN agents owner ON owner.id = d.owner_agent_id AND owner.seq = d.owner_seq WHERE d.status = 'started' AND d.note IS NULL AND json_extract(d.payload, '$.started_task_seq') = t.seq AND t.supervisor_id = owner.id AND t.status IN ('queued', 'preparing') AND a.retired_at IS NULL")?;
        Ok(stmt.query_map([], |r| Ok((r.get::<_, TaskId>(0)?, r.get::<_, AgentId>(1)?, r.get::<_, TaskStatus>(2)?, r.get::<_, i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await?;
    for (task, agent, status, id) in launches {
        if status == TaskStatus::Preparing {
            svc.spawn_runner(task, agent, crate::tasks::runner::RunnerStart::Prepare);
        } else {
            svc.schedule_reroute();
        }
        acknowledge_launch(svc, id).await?;
    }
    Ok(())
}

/// A durable dispatch acknowledgement avoids repeatedly postponing the keyed re-route timer.
pub(crate) async fn acknowledge_launch(svc: &TaskService, id: i64) -> Result<(), StoreError> {
    svc.write(move |tx, _| {
        tx.execute("UPDATE deferred_starts SET note = 'Launch dispatched.' WHERE id = ?1 AND status = 'started' AND note IS NULL", [id])?;
        Ok(())
    }).await
}

async fn fire(svc: &TaskService, row: &DeferredRow) -> Result<(), ToolFailure> {
    let saved: Saved = decode("deferred_starts.payload", &row.payload).map_err(|e| failed(&e))?;
    let trigger: StartTrigger =
        decode("deferred_starts.trigger_json", &row.trigger).map_err(|e| failed(&e))?;
    let caller = saved.caller(row);
    let id = row.id;
    let now = svc.clock().now();
    // Identity/scope checks happen even before the condition, so a retired owner cannot leave
    // intent dormant forever. check_record's condition gate is deferred until it is ready.
    let ready = if let StartTrigger::Commit { task } = &trigger {
        committed(svc, *task).await?.is_some()
    } else {
        let trigger = trigger.clone();
        svc.db()
            .call(move |conn| ready_in_store(conn, &trigger, now))
            .await
            .map_err(|e| failed(&e))?
    };
    // Independently verify identities while waiting; don't fail an unsatisfied condition.
    let saved_copy = saved.clone();
    let owner = row.owner;
    let owner_seq = row.owner_seq;
    let valid = svc.db().call(move |conn| {
        let active: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM agents WHERE id = ?1 AND seq = ?2 AND retired_at IS NULL)", params![owner, owner_seq], |r| r.get(0))?;
        if !active { return Ok(false); }
        for (task, seq) in saved_copy.pins { if pin(conn, task)? != Some(seq) { return Ok(false); } }
        Ok(true)
    }).await.map_err(|e| failed(&e))?;
    if !valid {
        return Err(refuse(
            "the scheduling owner or a pinned task was retired, forgotten or replaced",
        ));
    }
    if !ready {
        if let Some(task) = target(&trigger) {
            let closed = svc
                .db()
                .call(move |conn| {
                    Ok(qm_store::tasks::load(conn, task)?
                        .is_some_and(|r| r.status == TaskStatus::Closed))
                })
                .await
                .map_err(|e| failed(&e))?;
            if closed {
                return Err(refuse(format!(
                    "{task} closed before the deferred start's trigger was satisfied"
                )));
            }
        }
        return Ok(());
    }
    if let RecordReadiness::Waiting = svc
        .db()
        .call(move |conn| check_record(conn, id, &caller, now))
        .await
        .map_err(|e| failed(&e))??
    {
        return Ok(());
    }
    let how = StartAs {
        authority: saved.authority(),
        retry_tier: None,
        user_words: Some(saved.words),
        user_words_message: saved.words_message,
        deferred: Some(row.id),
    };
    start::start_when_ready(svc, &caller, saved.request, how).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
