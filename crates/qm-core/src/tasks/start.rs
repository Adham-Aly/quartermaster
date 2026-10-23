//! Starting a crewmate.
//!
//! ```text
//! tool call ──▶ A validate ──▶ B settle base, check delivery ──▶ record (one transaction) ──▶ TaskStarted
//!                                                                   │
//!                                           runner (background) ◀───┘ C worktree, setup, skills, brief, session
//! ```
//!
//! The tool returns after phase B, so every refusal (a bad base branch, a failed
//! fetch, a delivery gap) reaches the supervisor as the tool's answer; the slow part runs in the
//! task's runner, and its failures fail the task with a `task_failed` wake.
//!
//! A task routing can't serve yet stays `queued`; the re-route tick (`reroute_tick`) routes it
//! again every minute, and when it routes, settles the base again (freshly fetched) before
//! `preparing`.

use super::base::{RecordedBase, Starter, base_request};
use super::ports::{PORT_RANGE, choose as choose_ports};
use super::runner::RunnerStart;
use super::session::{RoutedSession, Workplace};
use super::{ProjectRow, TaskService, active_by_name, failed, load_project, refuse};
use crate::briefs::write_brief;
use crate::inbox::Notify;
use crate::routing::{CrewmateFacts, RouteOutcome, account_ref, route_crewmate};
use crate::skills::{Gathered, gather};
use jiff::Timestamp;
use qm_git::{BaseRequest, BaseSource, GitError, Repo, SettledBase};
use qm_policy::delivery::check_folder_writer;
use qm_policy::holds::{check_escalation, stronger_target};
use qm_policy::overrides::Authority;
use qm_policy::verification::choose as choose_level;
use qm_routing::decider::{Decided, DecidedBy};
use qm_routing::later_steps::{
    AppliedLaterStep, ChainLink, LaterStep as RoutedLaterStep, applicable, check_later_steps,
};
use qm_routing::rank::Needs;
use qm_routing::record::{BuildsOn, BuildsOnFact};
use qm_store::StoreError;
use qm_store::accounts::load_account;
use qm_store::agents::{self, NewCrewmate};
use qm_store::reports;
use qm_store::tasks::{self as task_store, NewTask, TaskRow};
use qm_tools::defs::{RoutingSummary, TaskStarted};
use qm_tools::handler::{Caller, StartRequest, ToolFailure};
use qm_types::data_dir::TaskPaths;
use qm_types::domain::{
    DeliveryMode, Effort, EngineKind, Provider, Role, StartedBy, TaskKind, TaskOutcome, TaskStatus,
    Tier, VerificationLevel, VerificationSource,
};
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, MessageId, ModelId, PortBlock, ProjectId, SecondmateId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long settling the base may take.
pub const BASE_TIMEOUT: Duration = Duration::from_mins(3);

/// `events.kind` of a started task.
pub const STARTED_EVENT: &str = "task.started";
/// `events.kind` of a task waiting for a model.
pub const QUEUED_EVENT: &str = "task.queued";
/// `events.kind` of listed files missing from your checkout.
pub const COPY_MISSING_EVENT: &str = "task.copy_missing";
/// `events.kind` of a finished setup command.
pub const SETUP_DONE_EVENT: &str = "task.setup_done";

/// A hook inside `start_crewmate`'s record transaction: called after the task row, its crewmate and
/// its routing decision are written, before the commit. A refusal rolls the whole start back and is
/// the tool's answer; wakes it posts are signalled after the commit. Verification uses it to move a
/// Careful builder `in_review` when its review task starts.
pub trait StartHook: Send + Sync + fmt::Debug {
    fn on_recorded(
        &self,
        conn: &Connection,
        task: TaskId,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<Result<(), Refusal>, StoreError>;
}

/// The hook that does nothing (the default until verification is wired).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoStartHook;

impl StartHook for NoStartHook {
    fn on_recorded(
        &self,
        conn: &Connection,
        task: TaskId,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<Result<(), Refusal>, StoreError> {
        let _ = (conn, task, notify, at);
        Ok(Ok(()))
    }
}

/// What a start may carry beyond the tool's arguments: the authority (a card choice for
/// [`TaskService::start_retry`]), the exact tier a retry routes at, and the words its brief
/// carries.
#[derive(Clone, Debug)]
pub(crate) struct StartAs {
    pub authority: Authority,
    /// A stronger-model retry routes at exactly this tier (the one the card showed).
    pub retry_tier: Option<Tier>,
    /// The user's words for the new task, replacing those inherited.
    pub user_words: Option<String>,
}

/// What routing needs again when a queued task is routed later, kept on its `task.started`
/// event: everything here was accepted when the task started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteInputs {
    pub named_model: Option<ModelId>,
    pub named_effort: Option<Effort>,
    pub premium: bool,
    pub stronger_retry: Option<Tier>,
    /// `user` (with `message`), `app_event` or `card`.
    pub authority: String,
    pub message: Option<MessageId>,
    pub builds_on: Option<BuildsOn>,
    pub later_step: Option<LaterStepRecord>,
    /// A review prefers another provider than its builder's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_provider_other_than: Option<Provider>,
}

/// An applied `later_steps` entry, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaterStepRecord {
    pub from: TaskId,
    pub message: Option<MessageId>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
}

impl RouteInputs {
    fn authority(&self) -> Authority {
        match (self.authority.as_str(), self.message) {
            ("card", _) => Authority::CardChoice,
            ("user", Some(message)) => Authority::Turn(StartedBy::User { message }),
            _ => Authority::Turn(StartedBy::AppEvent),
        }
    }

    fn later_step(&self) -> Option<AppliedLaterStep> {
        self.later_step.as_ref().map(|l| AppliedLaterStep {
            from: l.from,
            message: l.message,
            model: l.model.clone(),
            effort: l.effort,
        })
    }
}

fn authority_words(authority: Authority) -> (&'static str, Option<MessageId>) {
    match authority {
        Authority::CardChoice => ("card", None),
        Authority::Turn(StartedBy::User { message }) => ("user", Some(message)),
        Authority::Turn(StartedBy::AppEvent) => ("app_event", None),
    }
}

/// Who started the caller's turn; a caller without a turn is treated as an app-event turn.
fn caller_authority(caller: &Caller) -> Authority {
    Authority::Turn(caller.turn.map_or(StartedBy::AppEvent, |t| t.started_by))
}

/// The facts phase A gathered.
struct Validated {
    project: ProjectRow,
    builds_on: Option<BuildsOn>,
    retry: Option<(TaskRow, Tier)>,
    review_of: Option<TaskRow>,
    user_words: String,
    user_words_message: Option<MessageId>,
    /// The base to settle; `None` for a folder project, which has no branches.
    request: Option<BaseRequest>,
    later_step: Option<AppliedLaterStep>,
    secondmate_request: Option<i64>,
    /// A review's builder's provider, which the review prefers to avoid.
    builder_provider: Option<Provider>,
}

/// The provider `task`'s crewmate runs on, if it was routed.
fn provider_of(conn: &Connection, task: TaskId) -> Result<Option<Provider>, StoreError> {
    let Some(account) = agents::crewmate_of(conn, task)?.and_then(|a| a.account) else {
        return Ok(None);
    };
    Ok(load_account(conn, account)?.map(|a| a.provider))
}

/// Phase A and the facts it reads, on the database thread. No writes.
#[allow(clippy::too_many_lines)]
fn validate(
    conn: &Connection,
    caller: &Caller,
    req: &StartRequest,
    authority: Authority,
) -> Result<Result<Validated, Refusal>, StoreError> {
    // Later steps: each names a model or an effort, no kind twice.
    let steps: Vec<RoutedLaterStep> = req
        .later_steps
        .iter()
        .map(|s| RoutedLaterStep {
            kind: s.kind,
            model: s.model.clone(),
            effort: s.effort,
        })
        .collect();
    if let Err(r) = check_later_steps(&steps) {
        return Ok(Err(r));
    }

    // 2. The project.
    let Some(project) = active_by_name(conn, &req.project)? else {
        return Ok(Err(Refusal::new(format!(
            "There's no project called {}; use find_project_folder and propose_project",
            req.project.trim()
        ))));
    };
    // A secondmate's scope, and the request its task serves must be its own.
    if let Err(r) =
        crate::secondmates::scope::check_project_in(conn, caller, project.id, &project.name)?
    {
        return Ok(Err(r));
    }
    if caller.role == Role::Secondmate
        && let (Some(sm), Some(c)) = (caller.secondmate, req.correlation)
        && let Err(r) = crate::secondmates::scope::request_for(conn, sm, c)?
    {
        return Ok(Err(r));
    }

    // 3. builds_on: a fact, never a refusal beyond the task existing.
    let mut review_of = None;
    let builds_on = match req.builds_on {
        None => None,
        Some(target) => {
            let Some(row) = task_store::load(conn, target)? else {
                return Ok(Err(Refusal::new(format!(
                    "can't build on {target}: its details are unavailable because it closed and was forgotten or doesn't exist; use find_tasks to choose a task still kept, or include the needed facts in a new task"
                ))));
            };
            let report = reports::latest_clear(conn, target)?;
            let fact = match (row.kind, &report) {
                (TaskKind::Plan, Some(r))
                    if r.answer.as_deref().is_some_and(|a| !a.trim().is_empty()) =>
                {
                    BuildsOnFact::FinishedPlan
                }
                (TaskKind::Reproduce, Some(r)) if r.reproduced == Some(true) => {
                    BuildsOnFact::ConfirmedDiagnosis
                }
                (TaskKind::Research, Some(r)) if r.diagnosis => BuildsOnFact::ConfirmedDiagnosis,
                _ => BuildsOnFact::Nothing,
            };
            if req.kind == TaskKind::Review {
                if !row.kind.can_change_code() || row.head_commit.is_none() {
                    return Ok(Err(Refusal::new(format!(
                        "a review builds on code work with a commit to review, and {target} has none"
                    ))));
                }
                review_of = Some(row);
            }
            Some(BuildsOn { task: target, fact })
        }
    };
    if req.kind == TaskKind::Review && review_of.is_none() {
        return Ok(Err(Refusal::new(
            "a review task names the task it reviews in builds_on",
        )));
    }

    // 4. A stronger-model retry.
    let retry = match req.stronger_retry_of {
        None => None,
        Some(target) => {
            let Some(row) = task_store::load(conn, target)? else {
                return Ok(Err(Refusal::new(format!("{target} doesn't exist"))));
            };
            let retryable = matches!(
                row.status,
                TaskStatus::Held | TaskStatus::Stuck | TaskStatus::Failed | TaskStatus::Reported
            ) || (row.status == TaskStatus::Closed
                && row.outcome == Some(TaskOutcome::Failed));
            if !retryable {
                return Ok(Err(Refusal::new(format!(
                    "{target} is {}; a stronger-model retry is for held, stuck, failed or reported work",
                    row.status
                ))));
            }
            if let Err(r) = check_escalation(authority, row.escalations) {
                return Ok(Err(r));
            }
            let current = current_tier(conn, target)?;
            let available = available_tiers(conn)?;
            let Some(tier) = stronger_target(current, &available, row.escalations) else {
                return Ok(Err(Refusal::new(format!(
                    "no stronger model than {target}'s is available, so a stronger-model retry isn't possible"
                ))));
            };
            Some((row, tier))
        }
    };

    // 6. Your words.
    let (user_words, user_words_message) =
        if let Authority::Turn(StartedBy::User { message }) = authority {
            (
                qm_store::messages::text(conn, message)?.unwrap_or_default(),
                Some(message),
            )
        } else {
            {
                let from_retry = retry
                    .as_ref()
                    .map(|(r, _)| (r.user_words.clone(), r.user_words_message));
                let from_builds_on = match req.builds_on {
                    Some(t) => task_store::load(conn, t)?
                        .filter(|r| r.supervisor == caller.agent)
                        .map(|r| (r.user_words, r.user_words_message)),
                    None => None,
                };
                let from_request = match req.correlation {
                    Some(c) => request_words(conn, i64::from(c.get()))?.map(|w| (w, None)),
                    None => None,
                };
                from_retry
                    .or(from_builds_on)
                    .or(from_request)
                    .unwrap_or_default()
            }
        };

    // 7. The base request.
    let inherited = retry
        .as_ref()
        .map(|(r, _)| r)
        .or(review_of.as_ref())
        .map(|r| RecordedBase {
            typed: r.base_branch.clone(),
        });
    let starter = if caller.role == Role::Secondmate {
        let request = match req.correlation {
            Some(c) => request_branch(conn, i64::from(c.get()))?,
            None => None,
        };
        let standing = match caller.secondmate {
            Some(sm) => standing_branch(conn, sm)?,
            None => None,
        };
        (request, standing)
    } else {
        (None, None)
    };
    // A folder project has no branches, so there's no base to settle.
    let request = if project.delivery == DeliveryMode::Folder {
        if req.base_branch.is_some() {
            return Ok(Err(Refusal::new(
                "the personal folder has no branches, so a base branch can't be named",
            )));
        }
        None
    } else {
        match base_request(
            if caller.role == Role::Secondmate {
                Starter::Secondmate {
                    request: starter.0.as_deref(),
                    standing: starter.1.as_deref(),
                    typed: req.base_branch.as_deref(),
                }
            } else {
                Starter::Quartermaster {
                    authority,
                    typed: req.base_branch.as_deref(),
                }
            },
            inherited.as_ref(),
        ) {
            Ok(r) => Some(r),
            Err(r) => return Ok(Err(r)),
        }
    };

    // The applicable later step: nearest first up the builds_on chain.
    let chain = builds_on_chain(conn, req.builds_on)?;
    let links: Vec<ChainLink<'_>> = chain
        .iter()
        .map(|(task, steps, message)| ChainLink {
            task: *task,
            later_steps: steps,
            message: *message,
        })
        .collect();
    let later_step = applicable(req.kind, retry.is_some(), &links);
    let builder_provider = match &review_of {
        Some(row) => provider_of(conn, row.id)?,
        None => None,
    };

    Ok(Ok(Validated {
        project,
        builds_on,
        retry,
        review_of,
        user_words,
        user_words_message,
        request,
        later_step,
        secondmate_request: req.correlation.map(|c| i64::from(c.get())),
        builder_provider,
    }))
}

/// One task up the `builds_on` chain: its ID, its later steps and the message they came from.
type Link = (TaskId, Vec<RoutedLaterStep>, Option<MessageId>);

/// The `builds_on` chain from `first`, nearest first, with each task's later steps and the
/// message they came from (at most 16 links).
fn builds_on_chain(conn: &Connection, first: Option<TaskId>) -> Result<Vec<Link>, StoreError> {
    let mut out = Vec::new();
    let mut next = first;
    while let Some(task) = next {
        if out.len() >= 16 || out.iter().any(|(t, _, _)| *t == task) {
            break;
        }
        let Some(row) = task_store::load(conn, task)? else {
            break;
        };
        let steps: Vec<RoutedLaterStep> =
            serde_json::from_value(row.later_steps.clone()).unwrap_or_default();
        out.push((task, steps, row.user_words_message));
        next = row.builds_on;
    }
    Ok(out)
}

/// The tier the task last ran (or was meant to run) at.
pub(crate) fn current_tier(conn: &Connection, task: TaskId) -> Result<Tier, StoreError> {
    Ok(qm_store::routing_decisions::decisions_for_task(conn, task)?
        .into_iter()
        .next_back()
        .map_or(Tier::Medium, |(_, d)| {
            d.choice.map_or(d.tier_landed, |c| c.tier)
        }))
}

/// The tiers that have a model placed in them.
fn available_tiers(conn: &Connection) -> Result<Vec<Tier>, StoreError> {
    let mut tiers: Vec<Tier> = qm_store::models::list_placements(conn)?
        .into_iter()
        .map(|p| p.tier)
        .collect();
    tiers.sort_unstable();
    tiers.dedup();
    Ok(tiers)
}

fn request_branch(conn: &Connection, request: i64) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT base_branch FROM secondmate_requests WHERE id = ?1",
            params![request],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn request_words(conn: &Connection, request: i64) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT user_words FROM secondmate_requests WHERE id = ?1",
            params![request],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .filter(|w| !w.trim().is_empty()))
}

fn standing_branch(
    conn: &Connection,
    secondmate: SecondmateId,
) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT standing_base_branch FROM secondmates WHERE id = ?1",
            params![secondmate],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Settles the base on a blocking thread, bounded by [`BASE_TIMEOUT`] on the clock.
pub(crate) async fn settle(
    svc: &TaskService,
    project: &ProjectRow,
    request: BaseRequest,
) -> Result<SettledBase, ToolFailure> {
    let git = svc.deps().git.clone();
    let path = project.path.clone();
    let delivery = project.delivery;
    let default_branch = project.default_branch.clone();
    let work = tokio::task::spawn_blocking(move || {
        let repo = Repo::open(git, &path)?;
        repo.settle_base(&request, delivery, &default_branch)
    });
    let clock = svc.clock_arc();
    tokio::select! {
        joined = work => match joined {
            Ok(Ok(base)) => Ok(base),
            Ok(Err(GitError::BaseRefused(r))) => Err(refuse(format!("The task can't start: {r}"))),
            Ok(Err(e)) => Err(failed(&e)),
            Err(e) => Err(failed(&e)),
        },
        () = clock.sleep(BASE_TIMEOUT) => Err(refuse(
            "The task can't start: fetching origin took longer than 3 minutes",
        )),
    }
}

/// Picks the task's block of ports: what open tasks hold, and what listens now.
async fn pick_ports(svc: &TaskService) -> Result<PortBlock, ToolFailure> {
    let in_use = svc
        .db()
        .call(|conn| task_store::ports_in_use(conn))
        .await
        .map_err(|e| failed(&e))?;
    let sup = Arc::clone(&svc.deps().supervisor);
    let listeners = tokio::task::spawn_blocking(move || sup.listeners(PORT_RANGE))
        .await
        .map_err(|e| failed(&e))?
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "couldn't list listening ports; choosing from the store alone");
            Vec::new()
        });
    Ok(choose_ports(&in_use, &listeners)?)
}

/// What the start transaction decided.
enum Recorded {
    Started {
        task: TaskId,
        agent: AgentId,
        summary: RoutingSummary,
        routed: bool,
    },
    Refused(Refusal),
}

/// `start_crewmate`, phases A and B and the record. Spawns the runner (or the re-route tick).
pub async fn start(
    svc: &TaskService,
    caller: &Caller,
    req: StartRequest,
) -> Result<TaskStarted, ToolFailure> {
    let how = StartAs {
        authority: caller_authority(caller),
        retry_tier: None,
        user_words: None,
    };
    start_as(svc, caller, req, how).await
}

/// [`start`] with its authority, retry tier and words given.
#[allow(clippy::too_many_lines)]
pub(crate) async fn start_as(
    svc: &TaskService,
    caller: &Caller,
    req: StartRequest,
    how: StartAs,
) -> Result<TaskStarted, ToolFailure> {
    if svc.is_shutting_down() {
        return Err(refuse(
            "The app is quitting; start the task after it restarts",
        ));
    }
    // A routed request's overrides, applied with the routing turn's authority; otherwise the
    // authority this start was given (the caller's, or a retry's).
    let caller_copy = *caller;
    let (req, routed_authority) = svc
        .db()
        .call(move |conn| {
            let mut req = req;
            let authority =
                crate::secondmates::scope::apply_request_overrides(conn, &caller_copy, &mut req)?;
            Ok((req, authority))
        })
        .await
        .map_err(|e| failed(&e))?;
    let authority = routed_authority.unwrap_or(how.authority);
    let req_copy = req.clone();
    let mut validated = svc
        .db()
        .call(move |conn| validate(conn, &caller_copy, &req_copy, authority))
        .await
        .map_err(|e| failed(&e))??;
    if let (Some(tier), Some((_, landed))) = (how.retry_tier, validated.retry.as_mut()) {
        *landed = tier;
    }
    if let Some(words) = how.user_words {
        validated.user_words = words;
        validated.user_words_message = None;
    }

    // Code work on a plan waits for the plan: approved by the user, or finished under auto.
    if let Some(plan) = plan_built_on(svc, &req).await? {
        super::plan::ready_to_build(svc, plan)
            .map_err(|why| refuse(format!("The task can't start: {why}")))?;
    }

    // 5. The verification level (none for answer-only, review and build in place).
    let level = choose_level(
        req.kind,
        validated.project.delivery,
        req.verification,
        Some(validated.project.verification),
    );

    // Phase B: settle the base (network), check delivery.
    let mut base = match &validated.request {
        Some(request) => Some(settle(svc, &validated.project, request.clone()).await?),
        None => None,
    };
    if let (Some(reviewed), Some(base)) = (&validated.review_of, base.as_mut())
        && let Some(head) = &reviewed.head_commit
    {
        // A review starts at the reviewed task's head, on its base ref.
        base.commit = head.clone();
        if let Some(r) = &reviewed.base_ref {
            base.base_ref.clone_from(r);
        }
    }
    if req.kind.can_change_code() && validated.project.delivery == DeliveryMode::DirectPr {
        let readiness = svc
            .deps()
            .probe
            .probe(&validated.project, &validated.project.path)
            .await;
        if readiness.has_gaps() {
            return Err(refuse(format!(
                "The task can't start: {}",
                readiness.gaps.join("; ")
            )));
        }
    }
    let ports = pick_ports(svc).await?;

    // The record, in one transaction.
    let at = svc.clock().now();
    let config = svc.deps().config.borrow().clone();
    let rules = svc.deps().rules.clone();
    let health = Arc::clone(&svc.deps().health);
    let paths = svc.deps().data_dir.clone();
    let caller = *caller;
    let quick = level.is_some_and(|l| l.level == VerificationLevel::Quick);
    let route_inputs = {
        let (words, message) = authority_words(authority);
        RouteInputs {
            named_model: req.model.clone(),
            named_effort: req.effort,
            premium: req.premium,
            stronger_retry: validated.retry.as_ref().map(|(_, t)| *t),
            authority: words.to_owned(),
            message,
            builds_on: validated.builds_on.clone(),
            later_step: validated.later_step.as_ref().map(|l| LaterStepRecord {
                from: l.from,
                message: l.message,
                model: l.model.clone(),
                effort: l.effort,
            }),
            prefer_provider_other_than: validated.builder_provider,
        }
    };
    let hook = Arc::clone(&svc.deps().start_hook);
    let recorded = svc
        .db()
        .call(move |conn| {
            let tx = conn.transaction()?;
            let mut notify = Notify::new();
            // Folder work shares one folder, so one code-writing task at a time may be open in it.
            if let Err(r) = check_writer_slot(
                &tx,
                validated.project.id,
                validated.project.delivery,
                req.kind,
            )? {
                return Ok((Recorded::Refused(r), notify));
            }
            let new = NewTask {
                project: validated.project.id,
                supervisor: caller.agent,
                kind: req.kind,
                title: req.title.clone(),
                goal: req.goal.trim().to_owned(),
                user_words: validated.user_words.clone(),
                user_words_message: validated.user_words_message,
                instructions: serde_json::to_value(&req.instructions).unwrap_or_default(),
                constraints: serde_json::to_value(&req.constraints).unwrap_or_default(),
                tests_required: req.tests_required,
                builds_on: req.builds_on,
                parent: None,
                retry_of: validated.retry.as_ref().map(|(r, _)| r.id),
                escalations: validated
                    .retry
                    .as_ref()
                    .map_or(0, |(r, _)| r.escalations.saturating_add(1)),
                later_steps: serde_json::to_value(&req.later_steps).unwrap_or_default(),
                secondmate_request: validated.secondmate_request,
                delivery: validated.project.delivery,
                verification: level.map(|l| (l.level, l.source)),
                base_branch: base
                    .as_ref()
                    .and_then(SettledBase::typed_branch)
                    .map(str::to_owned),
                base_ref: base.as_ref().map(|b| b.base_ref.clone()),
                base_commit: base.as_ref().map(|b| b.commit.clone()),
                port_base: Some(ports.base),
                at,
            };
            let task = task_store::insert(&tx, &new)?;
            let home = match validated.project.delivery {
                DeliveryMode::Folder => validated.project.path.clone(),
                _ => paths.task(task).worktree,
            };
            let agent = agents::insert_crewmate(
                &tx,
                &NewCrewmate {
                    task,
                    supervisor: caller.agent,
                    secondmate: caller.secondmate,
                    home: &home,
                    at,
                },
            )?;
            let facts = facts_for(task, Some(agent), req.kind, &route_inputs, quick);
            let outcome = route_crewmate(&tx, &rules, &config, &facts, &health, at)?;
            let (summary, routed) = match outcome {
                RouteOutcome::Refused(r) => return Ok((Recorded::Refused(r), notify)),
                RouteOutcome::Routed(routed) => {
                    task_store::set_status(
                        &tx,
                        task,
                        TaskStatus::Queued,
                        TaskStatus::Preparing,
                        at,
                    )?;
                    let choice = routed.record.choice.clone();
                    if let Some(c) = &choice {
                        agents::set_routed(
                            &tx,
                            agent,
                            routed.provider.engine(),
                            c.account,
                            &routed.account_model,
                            c.effort,
                        )?;
                    }
                    (
                        RoutingSummary {
                            tier: routed
                                .record
                                .tier_used()
                                .unwrap_or(routed.record.tier_landed),
                            model: choice.as_ref().map(|c| c.model.clone()),
                            effort: choice.as_ref().map(|c| c.effort),
                            why: routed.record.why.clone(),
                        },
                        true,
                    )
                }
                RouteOutcome::Waiting { record, .. } => (
                    RoutingSummary {
                        tier: record.tier_landed,
                        model: None,
                        effort: None,
                        why: record.why.clone(),
                    },
                    false,
                ),
            };
            task_store::event(
                &tx,
                STARTED_EVENT,
                task,
                Some(agent),
                &format!("{task} started: {}", req.title),
                serde_json::json!({
                    "why": summary.why,
                    "route": route_inputs,
                    "base_ref": base.as_ref().map(|b| &b.base_ref),
                    "base_commit": base.as_ref().map(|b| &b.commit),
                    "ports": ports.base,
                }),
                at,
            )?;
            if !routed {
                task_store::event(
                    &tx,
                    QUEUED_EVENT,
                    task,
                    Some(agent),
                    &format!("{task} is waiting for a model: {}", summary.why),
                    serde_json::json!({}),
                    at,
                )?;
            }
            if let Err(r) = hook.on_recorded(&tx, task, &mut notify, at)? {
                return Ok((Recorded::Refused(r), Notify::new()));
            }
            tx.commit()?;
            Ok((
                Recorded::Started {
                    task,
                    agent,
                    summary,
                    routed,
                },
                notify,
            ))
        })
        .await
        .map_err(|e| failed(&e))?;
    let (recorded, notify) = recorded;
    svc.deps().inbox.notify(&notify);
    match recorded {
        Recorded::Refused(r) => Err(ToolFailure::Refused(r)),
        Recorded::Started {
            task,
            agent,
            summary,
            routed,
        } => {
            svc.changed();
            if routed {
                svc.spawn_runner(task, agent, RunnerStart::Prepare);
            } else {
                svc.schedule_reroute();
            }
            tracing::info!(%task, "task started");
            Ok(TaskStarted {
                task,
                branch: task.branch(),
                routing: summary,
            })
        }
    }
}

/// Routing's facts for a task, from what its start recorded.
fn facts_for(
    task: TaskId,
    agent: Option<AgentId>,
    kind: TaskKind,
    inputs: &RouteInputs,
    quick: bool,
) -> CrewmateFacts {
    CrewmateFacts {
        task,
        agent,
        authority: inputs.authority(),
        kind: Decided::Answer {
            value: kind,
            by: DecidedBy::Declared,
        },
        decider: DecidedBy::Declared,
        builds_on: inputs.builds_on.clone(),
        needs: Needs::default(),
        quick_verification: quick,
        named_model: inputs.named_model.clone(),
        named_effort: inputs.named_effort,
        premium: inputs.premium,
        stronger_retry: inputs.stronger_retry,
        later_step: inputs.later_step(),
        prefer_provider_other_than: inputs.prefer_provider_other_than,
    }
}

/// The settle request a queued task's base is settled with again: its recorded typed branch, or
/// the default.
fn recorded_request(row: &TaskRow) -> BaseRequest {
    match &row.base_branch {
        Some(branch) => BaseRequest::Typed {
            branch: branch.clone(),
            traced_to_user: true,
        },
        None => BaseRequest::Default,
    }
}

/// The re-route tick: routes each queued task again, oldest first, skipping those whose blockers
/// haven't finished; for each that routes, settles its base again and moves it to `preparing`.
/// Reschedules itself while any task stays queued.
pub(crate) async fn reroute_tick(svc: &TaskService) {
    if svc.is_shutting_down() {
        return;
    }
    let queued = match svc
        .db()
        .call(|conn| task_store::with_status(conn, TaskStatus::Queued))
        .await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the queued tasks");
            svc.schedule_reroute();
            return;
        }
    };
    let mut still_queued = false;
    for row in queued {
        match reroute_one(svc, row).await {
            Ok(true) => {}
            Ok(false) => still_queued = true,
            Err(e) => {
                still_queued = true;
                tracing::warn!(error = %e, "a queued task couldn't be routed again");
            }
        }
    }
    if still_queued {
        svc.schedule_reroute();
    }
}

/// Routes one queued task again; `Ok(true)` when it left the queue.
async fn reroute_one(svc: &TaskService, row: TaskRow) -> Result<bool, String> {
    let task = row.id;
    let rules = svc.deps().rules.clone();
    let health = Arc::clone(&svc.deps().health);
    let quick = row
        .verification
        .is_some_and(|(l, _)| l == VerificationLevel::Quick);
    let kind = row.kind;
    // Read the inputs and dry-run routing (rolled back: a waiting re-route records nothing).
    let config = svc.deps().config.borrow().clone();
    let now = svc.clock().now();
    let probe = svc
        .db()
        .call(move |conn| {
            let blockers = task_store::blockers_of(conn, task)?;
            let blocked = blockers.iter().any(|b| !blocker_finished(b.kind, b.status));
            if blocked {
                return Ok(None);
            }
            let inputs = route_inputs(conn, task)?;
            let agent = agents::crewmate_of(conn, task)?.map(|a| a.id);
            let tx = conn.transaction()?;
            let facts = facts_for(task, agent, kind, &inputs, quick);
            let routes = matches!(
                route_crewmate(&tx, &rules, &config, &facts, &health, now)?,
                RouteOutcome::Routed(_)
            );
            drop(tx);
            Ok(Some((routes, inputs, agent)))
        })
        .await
        .map_err(|e| e.to_string())?;
    let Some((true, inputs, Some(agent))) = probe else {
        return Ok(false);
    };
    let project = svc
        .db()
        .call(move |conn| load_project(conn, row.project))
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{task}'s project is gone"))?;
    let base = match settle(svc, &project, recorded_request(&row)).await {
        Ok(base) => base,
        Err(failure) => {
            // The base can't be settled any more: the task fails with the reason.
            let reason = failure.render();
            svc.fail(task, reason).await;
            return Ok(true);
        }
    };
    let at = svc.clock().now();
    let rules = svc.deps().rules.clone();
    let health = Arc::clone(&svc.deps().health);
    let config = svc.deps().config.borrow().clone();
    let moved = svc
        .write(move |tx, _| {
            let facts = facts_for(task, Some(agent), kind, &inputs, quick);
            let RouteOutcome::Routed(routed) =
                route_crewmate(tx, &rules, &config, &facts, &health, at)?
            else {
                return Ok(false);
            };
            let Some(c) = routed.record.choice.clone() else {
                return Ok(false);
            };
            task_store::set_base(
                tx,
                task,
                base.typed_branch().or(row.base_branch.as_deref()),
                &base.base_ref,
                &base.commit,
            )?;
            task_store::set_status(tx, task, TaskStatus::Queued, TaskStatus::Preparing, at)?;
            agents::set_routed(
                tx,
                agent,
                routed.provider.engine(),
                c.account,
                &routed.account_model,
                c.effort,
            )?;
            task_store::event(
                tx,
                "task.routed",
                task,
                Some(agent),
                &format!("{task} found a model: {}", routed.record.why),
                serde_json::json!({ "why": routed.record.why }),
                at,
            )?;
            Ok(true)
        })
        .await
        .map_err(|e| e.to_string())?;
    if moved {
        svc.spawn_runner(task, agent, RunnerStart::Prepare);
    }
    Ok(moved)
}

/// Whether a blocker counts as finished for a queued task: an answer-only task once reported, any
/// task once landed or closed.
pub(crate) fn blocker_finished(kind: TaskKind, status: TaskStatus) -> bool {
    match status {
        TaskStatus::Reported => kind.is_answer_only(),
        TaskStatus::Landed | TaskStatus::Closed => true,
        _ => false,
    }
}

/// The inputs a task's start recorded for routing.
fn route_inputs(conn: &Connection, task: TaskId) -> Result<RouteInputs, StoreError> {
    Ok(task_store::last_event(conn, task, STARTED_EVENT)?
        .and_then(|(_, payload)| payload.get("route").cloned())
        .and_then(|route| serde_json::from_value(route).ok())
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// Phase C
// ---------------------------------------------------------------------------------------------

/// Why phase C stopped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PrepareError {
    /// The task fails with this reason.
    #[error("{0}")]
    Failed(String),
    /// The task is being stopped or the app is quitting.
    #[error("preparing was stopped")]
    Cancelled,
}

/// What phase C produced, for opening the session.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub brief: String,
    pub gathered: Gathered,
    pub routed: RoutedSession,
    pub ports: PortBlock,
}

/// The routed session and ports of a task, from its agent row.
pub(crate) async fn routed_session(
    svc: &TaskService,
    task: TaskId,
) -> Result<(RoutedSession, PortBlock, TaskRow), PrepareError> {
    svc.db()
        .call(move |conn| {
            let row = task_store::load(conn, task)?;
            let agent = agents::crewmate_of(conn, task)?;
            let account = match agent.as_ref().and_then(|a| a.account) {
                Some(id) => load_account(conn, id)?,
                None => None,
            };
            Ok((row, agent, account))
        })
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))
        .and_then(|(row, agent, account)| {
            let row = row.ok_or_else(|| PrepareError::Failed(format!("{task} doesn't exist")))?;
            let agent =
                agent.ok_or_else(|| PrepareError::Failed(format!("{task} has no crewmate")))?;
            let (Some(model), Some(effort), Some(account)) = (agent.model, agent.effort, account)
            else {
                return Err(PrepareError::Failed(format!("{task} has no routed model")));
            };
            let ports = PortBlock {
                base: row
                    .port_base
                    .ok_or_else(|| PrepareError::Failed(format!("{task} has no ports")))?,
            };
            Ok((
                RoutedSession {
                    engine: account.provider.engine(),
                    account: account_ref(&account),
                    model,
                    effort,
                },
                ports,
                row,
            ))
        })
}

/// Where a crewmate works: the home its agent row was recorded with (its worktree, or a folder
/// task's folder), and whether its tools may write there.
pub(crate) async fn workplace(svc: &TaskService, task: TaskId) -> Result<Workplace, PrepareError> {
    let (row, agent) = svc
        .db()
        .call(move |conn| {
            Ok((
                task_store::load(conn, task)?,
                agents::crewmate_of(conn, task)?,
            ))
        })
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?;
    let row = row.ok_or_else(|| PrepareError::Failed(format!("{task} doesn't exist")))?;
    let agent = agent.ok_or_else(|| PrepareError::Failed(format!("{task} has no crewmate")))?;
    Ok(Workplace {
        dir: agent.home,
        writes_dir: row.delivery != DeliveryMode::Folder || row.kind.can_change_code(),
    })
}

/// Gathers the crewmate's instruction files and skills (idempotent).
pub(crate) async fn gather_for(svc: &TaskService, task: TaskId) -> Result<Gathered, PrepareError> {
    let home = svc.deps().home.clone();
    let skills = svc.deps().data_dir.task(task).skills;
    let dir = workplace(svc, task).await?.dir;
    tokio::task::spawn_blocking(move || gather(&home, &dir, &skills))
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?
        .map_err(|e| PrepareError::Failed(format!("The skills couldn't be gathered: {e}")))
}

/// The plan task a code-writing request builds on, if it builds on one.
async fn plan_built_on(
    svc: &TaskService,
    req: &StartRequest,
) -> Result<Option<TaskId>, ToolFailure> {
    let Some(target) = req.builds_on.filter(|_| req.kind.can_change_code()) else {
        return Ok(None);
    };
    let row = svc
        .db()
        .call(move |conn| task_store::load(conn, target))
        .await
        .map_err(|e| failed(&e))?;
    Ok(row.filter(|r| r.kind == TaskKind::Plan).map(|r| r.id))
}

/// Phase C: folders, worktree, setup, skills, brief. Every step picks up what an interrupted
/// attempt left; the session is opened by the runner.
#[allow(clippy::too_many_lines)]
pub(crate) async fn prepare(
    svc: &TaskService,
    task: TaskId,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Prepared, PrepareError> {
    let (routed, ports, row) = routed_session(svc, task).await?;
    let paths = svc.deps().data_dir.task(task);

    // 1. Folders.
    let gh = super::env::gh_dir(&paths);
    let mut dirs = vec![
        paths.root.clone(),
        paths.session.clone(),
        paths.artifacts.clone(),
        paths.evidence.clone(),
        paths.jobs.clone(),
        paths.skills.clone(),
        gh,
    ];
    // Claude Code keeps its own transcript and scratchpad; the provider engine's crewmate gets
    // these.
    let provider = routed.engine != EngineKind::Claude;
    if provider {
        dirs.push(paths.transcript.clone());
    }
    let scratchpad = provider.then(|| paths.scratchpad.clone());
    let scratch = svc.deps().data_dir.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        for dir in &dirs {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if let Some(dir) = &scratchpad {
            scratch.ensure_scratch()?;
            // Made private, every folder on the way checked: no link can redirect it.
            scratch.ensure_scratch_dir(dir)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| PrepareError::Failed(e.to_string()))?
    .map_err(|e| PrepareError::Failed(format!("The task's folders couldn't be made: {e}")))?;
    plan_folder(svc, task, &row, &paths).await?;

    // 2. The project, then the workplace: a folder task works in its folder, which must still be
    // there; any other task gets a worktree on its branch.
    let project = svc
        .db()
        .call(move |conn| load_project(conn, row.project))
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?
        .ok_or_else(|| PrepareError::Failed(format!("{task}'s project is gone")))?;
    if row.delivery == DeliveryMode::Folder {
        check_folder(task, &project.path).await?;
    } else {
        make_worktree(svc, task, &project, &row, &paths).await?;
    }

    // 3. The setup command, in the workplace, unless it already ran.
    let place = workplace(svc, task).await?;
    if let Some(command) = project
        .setup_command
        .clone()
        .filter(|c| !c.trim().is_empty())
    {
        let done = svc
            .db()
            .call(move |conn| task_store::last_event(conn, task, SETUP_DONE_EVENT))
            .await
            .map_err(|e| PrepareError::Failed(e.to_string()))?
            .is_some();
        if !done {
            match super::setup::run(svc, task, &paths, &place.dir, &command, ports, cancel).await {
                Ok(took) => {
                    let at = svc.clock().now();
                    let summary = format!(
                        "{task}: the setup command finished in {}",
                        crate::jobs::duration_words(took)
                    );
                    svc.write(move |tx, _| {
                        task_store::event(
                            tx,
                            SETUP_DONE_EVENT,
                            task,
                            None,
                            &summary,
                            serde_json::json!({ "seconds": took.as_secs() }),
                            at,
                        )
                        .map(drop)
                    })
                    .await
                    .map_err(|e| PrepareError::Failed(e.to_string()))?;
                }
                Err(super::setup::SetupError::Cancelled) => return Err(PrepareError::Cancelled),
                Err(e) => return Err(PrepareError::Failed(e.to_string())),
            }
        }
    }

    // 4. Skills and instruction files.
    let gathered = gather_for(svc, task).await?;

    // 5. The brief.
    let brief = write_brief(svc.db(), &svc.deps().data_dir, task)
        .await
        .map_err(|e| PrepareError::Failed(format!("The brief couldn't be written: {e}")))?;

    Ok(Prepared {
        brief: brief.text,
        gathered,
        routed,
        ports,
    })
}

/// Makes the task's worktree on its branch from its recorded base, copying the files the project
/// asks for. An interrupted attempt's worktree is picked up as it is.
async fn make_worktree(
    svc: &TaskService,
    task: TaskId,
    project: &ProjectRow,
    row: &TaskRow,
    paths: &TaskPaths,
) -> Result<(), PrepareError> {
    let (Some(base_ref), Some(base_commit)) = (row.base_ref.clone(), row.base_commit.clone())
    else {
        return Err(PrepareError::Failed(format!("{task} has no base recorded")));
    };
    let settled = SettledBase {
        source: if row.base_branch.is_some() {
            BaseSource::Typed
        } else {
            BaseSource::RemoteDefault
        },
        branch: row
            .base_branch
            .clone()
            .unwrap_or_else(|| project.default_branch.clone()),
        remote: project
            .github_remote
            .as_ref()
            .map(|_| qm_git::ORIGIN.to_owned()),
        base_ref,
        commit: base_commit,
    };
    let git = svc.deps().git.clone();
    let checkout = project.path.clone();
    let files = project.files_to_copy.clone();
    let wpaths = paths.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let repo = Repo::open(git, &checkout)?;
        repo.create_worktree(task, &wpaths, &settled, &files)
    })
    .await
    .map_err(|e| PrepareError::Failed(e.to_string()))?
    .map_err(|e| PrepareError::Failed(worktree_failure(task, &e)))?;
    if !prepared.reused && !prepared.copied.missing.is_empty() {
        let missing: Vec<String> = prepared
            .copied
            .missing
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let at = svc.clock().now();
        let summary = format!(
            "{task}: {} isn't in your checkout, so it wasn't copied",
            missing.join(", ")
        );
        let _ = svc
            .write(move |tx, _| {
                task_store::event(
                    tx,
                    COPY_MISSING_EVENT,
                    task,
                    None,
                    &summary,
                    serde_json::json!({ "missing": missing }),
                    at,
                )
                .map(drop)
            })
            .await;
    }
    Ok(())
}

/// The code-writing task already open in a project, if any: a second one is refused while it runs.
pub(crate) fn folder_writer(open: &[(TaskId, TaskKind)]) -> Option<TaskId> {
    open.iter()
        .find(|(_, kind)| kind.can_change_code())
        .map(|(task, _)| *task)
}

/// Whether a new task of `kind` may start in `project` now, given the tasks open in it. The outer
/// `Result` is the store's; the inner one is the refusal.
pub(crate) fn check_writer_slot(
    conn: &Connection,
    project: ProjectId,
    delivery: DeliveryMode,
    kind: TaskKind,
) -> Result<Result<(), Refusal>, StoreError> {
    let open = task_store::open_in_project(conn, project)?;
    Ok(check_folder_writer(delivery, kind, folder_writer(&open)))
}

/// A folder task's folder must still be there. Nothing is created or changed in it: it's yours.
pub(crate) async fn check_folder(task: TaskId, folder: &Path) -> Result<(), PrepareError> {
    let dir = folder.to_path_buf();
    let there = tokio::task::spawn_blocking(move || dir.is_dir())
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?;
    if there {
        Ok(())
    } else {
        Err(PrepareError::Failed(format!(
            "{task}'s folder {} isn't there, so nothing was started",
            folder.display()
        )))
    }
}

/// A worktree failure, said plainly.
fn worktree_failure(task: TaskId, e: &GitError) -> String {
    match e {
        GitError::BranchExists { branch, .. } => {
            format!("the branch {branch} already has commits of its own; nothing was touched")
        }
        GitError::WorktreePathOccupied { path, .. } => format!(
            "{} is already taken by something that isn't {task}'s worktree; nothing was touched",
            path.display()
        ),
        GitError::BranchCheckedOutElsewhere { branch, path } => format!(
            "the branch {branch} is checked out at {}; nothing was touched",
            path.display()
        ),
        other => format!("{task}'s worktree couldn't be made: {other}"),
    }
}

/// Choice 2 on a decision card: a new crewmate retrying `retried` at exactly `tier`, with the
/// authority of your card choice (`escalations + 1`, refused at two), the failed attempt's branch
/// and report in its brief (it names `retry_of`), and your words.
pub(crate) async fn start_retry(
    svc: &TaskService,
    retried: TaskId,
    tier: Tier,
    words: Option<String>,
) -> Result<TaskStarted, ToolFailure> {
    let loaded = svc
        .db()
        .call(move |conn| {
            let Some(row) = task_store::load(conn, retried)? else {
                return Ok(None);
            };
            let project = load_project(conn, row.project)?;
            let role = agents::role_of(conn, row.supervisor)?;
            let original = qm_store::crew_view::user_words(conn, retried)?;
            Ok(Some((row, project, role, original)))
        })
        .await
        .map_err(|e| failed(&e))?;
    let Some((row, project, role, original)) = loaded else {
        return Err(refuse(format!("{retried} doesn't exist")));
    };
    let Some(project) = project else {
        return Err(refuse(format!("{retried}'s project is gone")));
    };
    let (role, secondmate) = role.unwrap_or((Role::Quartermaster, None));
    let caller = Caller {
        agent: row.supervisor,
        role,
        turn: None,
        task: None,
        secondmate,
        plan: None,
    };
    let user_words = match words.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        Some(words) if original.trim().is_empty() => {
            format!("On the decision card for {retried}: {words}")
        }
        Some(words) => format!(
            "{}\n\nOn the decision card for {retried}: {words}",
            original.trim()
        ),
        None => original,
    };
    let verification = row
        .verification
        .filter(|(_, source)| *source == VerificationSource::User)
        .map(|(level, _)| level);
    let req = StartRequest {
        project: project.name.clone(),
        kind: row.kind,
        title: row.title.clone(),
        goal: row.goal.clone(),
        constraints: serde_json::from_value(row.constraints.clone()).unwrap_or_default(),
        tests_required: row.tests_required,
        // The retry builds on the failed attempt (`retry_of`), so its brief carries that
        // attempt's branch and report rather than what the attempt itself built on.
        builds_on: None,
        instructions: serde_json::from_value(row.instructions.clone()).unwrap_or_default(),
        later_steps: Vec::new(),
        model: None,
        effort: None,
        premium: false,
        base_branch: None,
        verification,
        stronger_retry_of: Some(retried),
        correlation: None,
    };
    let how = StartAs {
        authority: Authority::CardChoice,
        retry_tier: Some(tier),
        user_words: Some(user_words),
    };
    start_as(svc, &caller, req, how).await
}

/// A plan task's plan folder, made with its state; a build on a plan gets its own copy of the
/// plan and its assets, so the plan task can close while the build runs. Both stay under the
/// scratch root.
async fn plan_folder(
    svc: &TaskService,
    task: TaskId,
    row: &TaskRow,
    paths: &TaskPaths,
) -> Result<(), PrepareError> {
    let source = match row.builds_on.filter(|_| row.kind.can_change_code()) {
        Some(target) => svc
            .db()
            .call(move |conn| task_store::load(conn, target))
            .await
            .map_err(|e| PrepareError::Failed(e.to_string()))?
            .filter(|r| r.kind == TaskKind::Plan)
            .map(|r| svc.plan_dir(r.id)),
        None => None,
    };
    let is_plan = row.kind == TaskKind::Plan;
    let dir = paths.plan.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if is_plan {
            crate::plans::create(&dir)?;
        } else if let Some(from) = source
            && !dir.join(qm_plans::PLAN_FILE).is_file()
        {
            crate::plans::copy_for_build(&from, &dir)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| PrepareError::Failed(e.to_string()))?
    .map_err(|e| PrepareError::Failed(format!("{task}'s plan folder couldn't be made: {e}")))
}
