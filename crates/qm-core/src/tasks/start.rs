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
use crate::places::{self, Place, Resolver};
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
use qm_types::ids::{
    AgentId, CommitSha, MessageId, ModelId, PortBlock, SecondmateId, TaskId, TaskLabelId,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    pub user_words_message: Option<MessageId>,
    /// Consumed atomically with the task record, never before.
    pub deferred: Option<i64>,
}

/// What routing needs again when a queued task is routed later, kept on its `task.started`
/// event: everything here was accepted when the task started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RouteInputs {
    pub named_model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<qm_types::model_preference::ModelPreference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_tier: Option<Tier>,
    pub named_effort: Option<Effort>,
    pub premium: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_message: Option<MessageId>,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premium: bool,
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
            premium: l.premium,
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
pub(crate) fn caller_authority(caller: &Caller) -> Authority {
    Authority::Turn(caller.turn.map_or(StartedBy::AppEvent, |t| t.started_by))
}

/// The facts phase A gathered.
struct Validated {
    /// The project the place gets: as it is, as it would come back, or as it would be added.
    project: ProjectRow,
    /// Where the task works, and whether it works there in place.
    place: Place,
    in_place: bool,
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
    found: (Place, &Path, Timestamp),
    frozen_base: Option<BaseRequest>,
) -> Result<Result<Validated, Refusal>, StoreError> {
    // Later steps: each names a model, an effort or premium, no kind twice, and a named model is
    // placed in some tier (premium in premium).
    let steps: Vec<RoutedLaterStep> = req
        .later_steps
        .iter()
        .map(|s| RoutedLaterStep {
            kind: s.kind,
            model: s.model.clone(),
            effort: s.effort,
            premium: s.premium,
        })
        .collect();
    let mut placed_tiers = HashMap::new();
    for model in steps.iter().filter_map(|s| s.model.as_ref()) {
        placed_tiers.insert(model.clone(), crate::routing::tiers_of(conn, model)?);
    }
    if let Err(r) = check_later_steps(&steps, |m| placed_tiers.get(m).cloned().unwrap_or_default())
    {
        return Ok(Err(r));
    }

    // 2. The project: the place's, registered when the task is recorded. A plain folder is always
    // worked in place; a git project only when your words asked for it.
    let (place, home, at) = found;
    let project = places::preview(conn, &place, home, at)?;
    let in_place = req.in_place || project.delivery == DeliveryMode::Folder;
    if in_place && req.kind == TaskKind::Review {
        return Ok(Err(Refusal::new(
            "a review works in a worktree at the reviewed head, so it can't work in place",
        )));
    }
    // The request a secondmate's task serves must be its own.
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
                // Any kind may confirm a diagnosis.
                (_, Some(r)) if r.reproduced == Some(true) || r.diagnosis => {
                    BuildsOnFact::ConfirmedDiagnosis
                }
                _ => BuildsOnFact::Nothing,
            };
            if req.kind == TaskKind::Review {
                if row.project != project.id {
                    return Ok(Err(Refusal::new(format!(
                        "a review must use the same repository as {target}; start it in that task's project"
                    ))));
                }
                if !row.kind.can_change_code() || row.base_commit.is_none() {
                    return Ok(Err(Refusal::new(format!(
                        "a review builds on code work with a commit to review, and {target} has none"
                    ))));
                }
                // A local commit can precede report (and hence head_commit). Phase B reads
                // the actual task-owned commit; never substitute an inherited branch tip.
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
    let request = if let Some(base) = frozen_base {
        if in_place && req.base_branch.is_some() {
            return Ok(Err(Refusal::new(
                "a task working in place has no branches, so a base branch can't be named",
            )));
        }
        if in_place { None } else { Some(base) }
    } else {
        match select_base(conn, caller, req, authority, in_place, inherited.as_ref())? {
            Ok(base) => base,
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
    let named_now =
        req.model.is_some() || req.tier.is_some() || req.effort.is_some() || req.premium;
    let later_step = applicable(req.kind, retry.is_some(), named_now, &links);
    let builder_provider = match &review_of {
        Some(row) => provider_of(conn, row.id)?,
        None => None,
    };

    Ok(Ok(Validated {
        project,
        place,
        in_place,
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

/// Freeze a secondmate's authorized base at deferred registration, using the ordinary selector.
pub(crate) fn freeze_base(
    conn: &Connection,
    caller: &Caller,
    req: &StartRequest,
    authority: Authority,
) -> Result<Result<Option<BaseRequest>, Refusal>, StoreError> {
    let source = req.stronger_retry_of.or_else(|| {
        (req.kind == TaskKind::Review)
            .then_some(req.builds_on)
            .flatten()
    });
    let inherited = match source {
        Some(task) => task_store::load(conn, task)?.map(|row| RecordedBase {
            typed: row.base_branch,
        }),
        None => None,
    };
    select_base(
        conn,
        caller,
        req,
        authority,
        req.in_place,
        inherited.as_ref(),
    )
}

/// Select and validate a scoped base without settling git or allocating a task.
fn select_base(
    conn: &Connection,
    caller: &Caller,
    req: &StartRequest,
    authority: Authority,
    in_place: bool,
    inherited: Option<&RecordedBase>,
) -> Result<Result<Option<BaseRequest>, Refusal>, StoreError> {
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
    // In-place work has no branches, so there's no base to settle.
    if in_place {
        if req.base_branch.is_some() {
            return Ok(Err(Refusal::new(
                "a task working in place has no branches, so a base branch can't be named",
            )));
        }
        Ok(Ok(None))
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
            inherited,
        ) {
            Ok(r) => Ok(Ok(Some(r))),
            Err(r) => Ok(Err(r)),
        }
    }
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

pub(crate) fn standing_branch(
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

/// The branch names a new task may take, from its title, in order of preference: the plain name,
/// then `-2` up to `-99`, less those a local or `origin/` branch already has. The store drops the
/// ones other tasks recorded. `None` for a folder task, which has no branch. If the checkout can't
/// be read, every candidate stays: preparing checks again before it makes the branch.
pub(crate) async fn free_branch_names(
    svc: &TaskService,
    project: &ProjectRow,
    title: &str,
) -> Option<Vec<String>> {
    if project.delivery == DeliveryMode::Folder {
        return None;
    }
    let candidates = qm_policy::names::branch_candidates(title);
    let git = svc.deps().git.clone();
    let checkout = project.path.clone();
    let names = candidates.clone();
    let taken = tokio::task::spawn_blocking(move || {
        Repo::open(git, &checkout).and_then(|repo| repo.taken_branch_names(&names))
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|read| read.map_err(|e| e.to_string()));
    match taken {
        Ok(taken) => Some(
            candidates
                .into_iter()
                .filter(|name| !taken.contains(name))
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%error, "the checkout's branches couldn't be read");
            Some(candidates)
        }
    }
}

/// Every name from the title's branch to its `-99` is in use.
pub(crate) fn no_free_branch(title: &str) -> Refusal {
    let first = qm_policy::names::branch_for_title(title);
    Refusal::new(format!(
        "Every branch name from {first} to {first}-99 is taken; give the task a different title."
    ))
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
        label: TaskLabelId,
        branch: Option<String>,
        agent: AgentId,
        summary: RoutingSummary,
        routed: bool,
    },
    Refused(Refusal),
    DeferredWaiting,
}

/// An unsatisfied deferred trigger is not a tool failure.
pub(crate) enum StartOutcome {
    Started(TaskStarted),
    DeferredWaiting,
}

/// The place `req` names: a listed project's folder, a folder (its project found by the git-root
/// rule), or your home folder. With `create`, a folder that doesn't exist yet is made first, with
/// any missing parents. A repository new to the app has its legacy task numbers reserved.
pub(crate) async fn find_place(
    svc: &TaskService,
    project: Option<String>,
    folder: Option<String>,
    create: bool,
) -> Result<Place, ToolFailure> {
    if let Some(name) = project {
        let lookup = name.clone();
        let row = svc
            .db()
            .call(move |conn| active_by_name(conn, &lookup))
            .await
            .map_err(|e| failed(&e))?;
        let Some(row) = row else {
            return Err(refuse(format!(
                "There's no listed project called {name}; name its folder instead"
            )));
        };
        // Its folder is read again: it may have changed kind since it was registered.
        let (home, git, env) = (
            svc.deps().home.clone(),
            svc.deps().git.clone(),
            svc.deps().inherited_env.clone(),
        );
        let data = svc.deps().data_dir.root().to_owned();
        return tokio::task::spawn_blocking(move || {
            Resolver {
                home: &home,
                data_dir: &data,
                git: git.program(),
                env: &env,
            }
            .at_root(row.path)
        })
        .await
        .map_err(|e| ToolFailure::Failed(e.to_string()));
    }
    let text = folder.unwrap_or_else(|| "~".to_owned());
    let plain = svc
        .db()
        .call(|conn| places::plain_folders(conn))
        .await
        .map_err(|e| failed(&e))?;
    let home = svc.deps().home.clone();
    let data = svc.deps().data_dir.root().to_owned();
    let git = svc.deps().git.clone();
    let env = svc.deps().inherited_env.clone();
    let place = tokio::task::spawn_blocking(move || {
        let resolver = Resolver {
            home: &home,
            data_dir: &data,
            git: git.program(),
            env: &env,
        };
        if create {
            resolver.create(&text)?;
        }
        resolver.resolve(&text, &plain)
    })
    .await
    .map_err(|e| ToolFailure::Failed(e.to_string()))?
    .map_err(refuse)?;
    if place.repo.is_some() {
        let root = place.root.clone();
        let known = svc
            .db()
            .call(move |conn| {
                Ok(qm_store::projects::active_by_path(conn, &root)?.is_some()
                    || qm_store::projects::retired_by_path(conn, &root)?.is_some())
            })
            .await
            .map_err(|e| failed(&e))?;
        if !known {
            let git = svc.deps().git.clone();
            let checkouts = vec![place.root.clone()];
            let scan = tokio::task::spawn_blocking(move || {
                super::startup::branch_task_numbers(&git, &checkouts)
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
            match scan {
                Ok(numbers) => qm_store::ids::reserve("tasks", numbers),
                Err(reason) => {
                    tracing::warn!(%reason, "a new repository's branches weren't scanned");
                }
            }
        }
    }
    Ok(place)
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
        user_words_message: None,
        deferred: None,
    };
    start_as(svc, caller, req, how).await
}

/// [`start`] with its authority, retry tier and words given (non-deferred callers).
pub(crate) async fn start_as(
    svc: &TaskService,
    caller: &Caller,
    req: StartRequest,
    how: StartAs,
) -> Result<TaskStarted, ToolFailure> {
    assert!(
        how.deferred.is_none(),
        "use start_when_ready for deferred starts"
    );
    match start_when_ready(svc, caller, req, how).await? {
        StartOutcome::Started(started) => Ok(started),
        StartOutcome::DeferredWaiting => unreachable!("only deferred starts wait"),
    }
}

/// The ordinary start path, allowing deferred intent to remain pending after settlement.
#[allow(clippy::too_many_lines)]
pub(crate) async fn start_when_ready(
    svc: &TaskService,
    caller: &Caller,
    req: StartRequest,
    how: StartAs,
) -> Result<StartOutcome, ToolFailure> {
    if svc.is_shutting_down() {
        return Err(refuse(
            "The app is quitting; start the task after it restarts",
        ));
    }
    // Check typed choices before inheritance: a routed source cannot authorize arbitrary fields
    // the secondmate supplies. Citations never replace this turn's authority for other overrides.
    let caller_copy = *caller;
    let (req, frozen_base) = svc
        .db()
        .call(move |conn| {
            let mut req = req;
            if let Some(id) = how.deferred
                && let Some(base) = crate::deferred_starts::frozen_request(conn, id, &caller_copy, &req)?
            {
                return Ok(Ok((req, base.base)));
            }
            let explicit_choice = req.model.is_some() || req.tier.is_some() || req.effort.is_some() || req.premium;
            if explicit_choice || req.preference.is_some() {
                req.choice_message = req.choice_message.or(match how.authority {
                    Authority::Turn(StartedBy::User { message }) => Some(message),
                    _ => None,
                });
                if let Some(message) = req.choice_message {
                    if let Err(r) = qm_store::user_message_sources::verify(conn, caller_copy.agent, message)? {
                        return Ok(Err(r.into()));
                    }
                } else if explicit_choice && how.authority != Authority::CardChoice {
                    return Ok(Err(refuse("model, tier, effort and premium choices need the user's message; cite it with choice_message")));
                }
            }
            let authority =
                crate::secondmates::scope::apply_request_overrides(conn, &caller_copy, &mut req)?;
            Ok(authority.map(|_| (req, None)).map_err(ToolFailure::from))
        })
        .await
        .map_err(|e| failed(&e))??;
    let authority = how.authority;
    let place = find_place(
        svc,
        req.project.clone(),
        req.folder.clone(),
        req.create_folder,
    )
    .await?;
    let req_copy = req.clone();
    let home = svc.deps().home.clone();
    let now = svc.clock().now();
    let mut validated = svc
        .db()
        .call(move |conn| {
            validate(
                conn,
                &caller_copy,
                &req_copy,
                authority,
                (place, &home, now),
                frozen_base,
            )
        })
        .await
        .map_err(|e| failed(&e))??;
    let in_place = validated.in_place;
    let delivery = if in_place {
        DeliveryMode::Folder
    } else {
        validated.project.delivery
    };
    if let (Some(tier), Some((_, landed))) = (how.retry_tier, validated.retry.as_mut()) {
        *landed = tier;
    }
    if let Some(words) = how.user_words {
        validated.user_words = words;
        validated.user_words_message = how.user_words_message;
    }

    // Code work on a plan waits for the plan: approved by the user, or finished under auto.
    if let Some(plan) = plan_built_on(svc, &req).await? {
        super::plan::ready_to_build(svc, &plan)
            .map_err(|why| refuse(format!("The task can't start: {why}")))?;
    }

    // 5. The verification level (every task has one, except build in place).
    let level = choose_level(
        delivery,
        req.verification,
        Some(validated.project.verification),
    );

    // Freeze the observed source before any asynchronous base settlement. This deliberately
    // does not write the builder's reported head: a local commit is not a report.
    let review_head = if let Some(reviewed) = &validated.review_of {
        Some(crate::deferred_starts::committed(svc, reviewed.id).await?.ok_or_else(|| {
            refuse(format!(
                "{} has no task-owned commit to review; commit the code locally, then start its review",
                reviewed.id
            ))
        })?)
    } else {
        None
    };
    // Phase B: settle the base (network), check delivery.
    let mut base = match &validated.request {
        Some(request) => Some(settle(svc, &validated.project, request.clone()).await?),
        None => None,
    };
    if let (Some(reviewed), Some(base)) = (&validated.review_of, base.as_mut())
        && let Some(head) = &review_head
    {
        // A review starts at the reviewed task's head, on its base ref.
        base.commit = head.clone();
        if let Some(r) = &reviewed.base_ref {
            base.base_ref.clone_from(r);
        }
    }
    if req.kind.can_change_code() && delivery == DeliveryMode::DirectPr {
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
    let branch_names = if in_place {
        None
    } else {
        free_branch_names(svc, &validated.project, &req.title).await
    };
    let home = svc.deps().home.clone();

    // The record, in one transaction.
    let at = svc.clock().now();
    let config = svc.deps().config.borrow().clone();
    let health = Arc::clone(&svc.deps().health);
    let paths = svc.deps().data_dir.clone();
    let caller = *caller;
    let quick = level.is_some_and(|l| l.level == VerificationLevel::Quick);
    let route_inputs = {
        let (words, message) = authority_words(authority);
        RouteInputs {
            named_model: req.model.clone(),
            preference: req.preference.clone(),
            named_tier: req.tier,
            named_effort: req.effort,
            premium: req.premium,
            choice_message: req.choice_message,
            stronger_retry: validated.retry.as_ref().map(|(_, t)| *t),
            authority: words.to_owned(),
            message,
            builds_on: validated.builds_on.clone(),
            later_step: validated.later_step.as_ref().map(|l| LaterStepRecord {
                from: l.from,
                message: l.message,
                model: l.model.clone(),
                effort: l.effort,
                premium: l.premium,
            }),
            prefer_provider_other_than: validated.builder_provider,
        }
    };
    let hook = Arc::clone(&svc.deps().start_hook);
    let deferred = how.deferred;
    let shutting_down = svc.inner.shutting_down.subscribe();
    let recorded = svc
        .db()
        .call(move |conn| {
            let tx = conn.transaction()?;
            let mut notify = Notify::new();
            if deferred.is_some() && *shutting_down.borrow() {
                return Ok((
                    Recorded::Refused(Refusal::new(
                        "the app is quitting; deferred intent remains pending",
                    )),
                    notify,
                ));
            }
            if let Some(id) = deferred {
                use crate::deferred_starts::RecordReadiness;
                match crate::deferred_starts::check_record(&tx, id, &caller, at)? {
                    Err(r) => return Ok((Recorded::Refused(r), notify)),
                    Ok(RecordReadiness::Waiting) => return Ok((Recorded::DeferredWaiting, notify)),
                    Ok(RecordReadiness::Ready) => {}
                }
            }
            // In-place work shares its folder, so one code-kind task at a time may work there.
            if let Err(r) = check_writer_slot(&tx, in_place, &validated.place.folder, req.kind)? {
                return Ok((Recorded::Refused(r), notify));
            }
            let project = places::register(&tx, &validated.place, &home, at)?;
            // Recheck the repository binding after asynchronous settlement; keep the frozen
            // SHA, but never launch it in a project the source no longer belongs to.
            if let Some(reviewed) = &validated.review_of
                && task_store::load(&tx, reviewed.id)?.is_none_or(|source| {
                    source.project != project.id || source.created_at != reviewed.created_at
                }) {
                return Ok((Recorded::Refused(Refusal::new(
                    "the reviewed task changed repository or identity; start its review again in the same repository"
                )), notify));
            }
            let branch = match &branch_names {
                None => None,
                Some(names) => match task_store::first_free_branch(&tx, names)? {
                    Some(name) => Some(name),
                    None => return Ok((Recorded::Refused(no_free_branch(&req.title)), notify)),
                },
            };
            // Deferred words can outlive their chat row. This link is optional metadata,
            // not choice authority; check it in the insert transaction, after settlement.
            if let Some(message) = validated.user_words_message
                && qm_store::messages::get(&tx, message)?.is_none()
            {
                validated.user_words_message = None;
            }
            if let Some(label) = req.label
                && let Some(why) = join_refusal(&tx, &caller, label, "join_task")?
            {
                return Ok((Recorded::Refused(why), notify));
            }
            let new = NewTask {
                label: req.label,
                branch: branch.clone(),
                project: project.id,
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
                delivery,
                in_place,
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
            let workplace = if in_place {
                validated.place.folder.clone()
            } else {
                paths.worktree(task)
            };
            let agent = agents::insert_crewmate(
                &tx,
                &NewCrewmate {
                    task,
                    supervisor: caller.agent,
                    secondmate: caller.secondmate,
                    home: &workplace,
                    at,
                },
            )?;
            // A secondmate's projects are its open crewmates'.
            if let Some(sm) = caller.secondmate {
                qm_store::secondmates::sync_projects(&tx, sm)?;
            }
            if let Some(id) = deferred
                && !crate::deferred_starts::record_started(&tx, id, task, at)?
            {
                return Ok((
                    Recorded::Refused(Refusal::new(
                        "the deferred start was cancelled or already started",
                    )),
                    Notify::new(),
                ));
            }
            let facts = facts_for(task, Some(agent), req.kind, &route_inputs, quick);
            let outcome = route_crewmate(&tx, &config, &facts, &health, at)?;
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
            let label = task_store::label(&tx, task)?.unwrap_or_else(|| task.own_label());
            tx.commit()?;
            Ok((
                Recorded::Started {
                    task,
                    label,
                    branch,
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
        Recorded::DeferredWaiting => Ok(StartOutcome::DeferredWaiting),
        Recorded::Started {
            task,
            label,
            branch,
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
            if let Some(id) = deferred {
                crate::deferred_starts::acknowledge_launch(svc, id)
                    .await
                    .map_err(|e| failed(&e))?;
            }
            tracing::info!(%task, "task started");
            Ok(StartOutcome::Started(TaskStarted {
                crewmate: task,
                task: label,
                branch,
                routing: summary,
            }))
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
        preference: inputs.preference.clone(),
        named_tier: inputs.named_tier,
        named_effort: inputs.named_effort,
        premium: inputs.premium,
        choice_message: inputs.choice_message,
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

/// A dry run of routing a queued task (rolled back: a waiting re-route records nothing): `None`
/// while a blocker is unfinished, else whether it routes (a refusal when it can't run as asked),
/// with its inputs and crewmate.
type Probe = Option<(Result<bool, Refusal>, RouteInputs, Option<AgentId>)>;

async fn probe_route(svc: &TaskService, row: &TaskRow) -> Result<Probe, String> {
    let task = row.id;
    let health = Arc::clone(&svc.deps().health);
    let quick = row
        .verification
        .is_some_and(|(l, _)| l == VerificationLevel::Quick);
    let kind = row.kind;
    let config = svc.deps().config.borrow().clone();
    let now = svc.clock().now();
    svc.db()
        .call(move |conn| {
            let blockers = task_store::blockers_of(conn, task)?;
            let mut blocked = false;
            for b in &blockers {
                blocked |= !blocker_finished(conn, b.task)?;
            }
            if blocked {
                return Ok(None);
            }
            let inputs = route_inputs(conn, task)?;
            let agent = agents::crewmate_of(conn, task)?.map(|a| a.id);
            let tx = conn.transaction()?;
            let facts = facts_for(task, agent, kind, &inputs, quick);
            let routes = match route_crewmate(&tx, &config, &facts, &health, now)? {
                RouteOutcome::Routed(_) => Ok(true),
                RouteOutcome::Waiting { .. } => Ok(false),
                RouteOutcome::Refused(refusal) => Err(refusal),
            };
            drop(tx);
            Ok(Some((routes, inputs, agent)))
        })
        .await
        .map_err(|e| e.to_string())
}

/// Routes one queued task again; `Ok(true)` when it left the queue.
async fn reroute_one(svc: &TaskService, row: TaskRow) -> Result<bool, String> {
    let task = row.id;
    let quick = row
        .verification
        .is_some_and(|(l, _)| l == VerificationLevel::Quick);
    let kind = row.kind;
    let probe = probe_route(svc, &row).await?;
    // The model it was started with left every tier while it waited: it can't run as asked.
    if let Some((Err(refusal), _, _)) = probe {
        svc.fail(task, refusal.reason).await;
        return Ok(true);
    }
    let Some((Ok(true), inputs, Some(agent))) = probe else {
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
    // Re-routing refreshes ordinary bases, never a review's frozen source snapshot.
    let mut base = base;
    if row.kind == TaskKind::Review {
        base.commit = row
            .base_commit
            .clone()
            .ok_or_else(|| format!("{task}'s reviewed SHA is gone"))?;
        base.base_ref = row
            .base_ref
            .clone()
            .ok_or_else(|| format!("{task}'s reviewed base is gone"))?;
    }
    let at = svc.clock().now();
    let health = Arc::clone(&svc.deps().health);
    let config = svc.deps().config.borrow().clone();
    let moved = svc
        .write(move |tx, _| {
            let facts = facts_for(task, Some(agent), kind, &inputs, quick);
            let RouteOutcome::Routed(routed) = route_crewmate(tx, &config, &facts, &health, at)?
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

/// Whether a blocker counts as finished for a queued task: a task that reported an answer, any task
/// once landed or closed.
pub(crate) fn blocker_finished(conn: &Connection, blocker: TaskId) -> Result<bool, StoreError> {
    let Some(row) = task_store::load(conn, blocker)? else {
        return Ok(false);
    };
    Ok(match row.status {
        TaskStatus::Reported => {
            super::delivered::of_task(conn, &row)? == qm_policy::delivery::Deliverable::Answer
        }
        TaskStatus::Landed | TaskStatus::Closed => true,
        _ => false,
    })
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
/// task's folder).
pub(crate) async fn workplace(svc: &TaskService, task: TaskId) -> Result<Workplace, PrepareError> {
    let agent = svc
        .db()
        .call(move |conn| agents::crewmate_of(conn, task))
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?;
    let agent = agent.ok_or_else(|| PrepareError::Failed(format!("{task} has no crewmate")))?;
    Ok(Workplace { dir: agent.home })
}

/// Why `caller` can't put a crewmate in `label`, named in the argument `field`: it doesn't exist,
/// or (for a secondmate) holds none of its crewmates. `None` when it can.
pub(crate) fn join_refusal(
    conn: &Connection,
    caller: &Caller,
    label: TaskLabelId,
    field: &str,
) -> Result<Option<Refusal>, StoreError> {
    let crewmates = qm_store::task_labels::crewmates(conn, label)?;
    if crewmates.is_empty() {
        return Ok(Some(Refusal::new(format!(
            "{label} doesn't exist; leave {field} out to give the crewmate a task of its own"
        ))));
    }
    if crewmates.iter().all(|(_, open)| !open) {
        return Ok(Some(Refusal::new(format!(
            "{label} is finished: its crewmates are closed; leave {field} out to give the \
             crewmate a task of its own"
        ))));
    }
    if caller.role == Role::Secondmate {
        let mut supervised = false;
        for (crewmate, _) in &crewmates {
            if task_store::load(conn, *crewmate)?.is_some_and(|r| r.supervisor == caller.agent) {
                supervised = true;
                break;
            }
        }
        if !supervised {
            return Ok(Some(Refusal::new(format!(
                "{label} holds none of your crewmates; name one of your own tasks, or leave \
                 {field} out"
            ))));
        }
    }
    Ok(None)
}

/// Gathers the crewmate's instruction files and skills (idempotent).
pub(crate) async fn gather_for(svc: &TaskService, task: TaskId) -> Result<Gathered, PrepareError> {
    let home = svc.deps().home.clone();
    let skills = svc
        .paths(task)
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?
        .skills;
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
) -> Result<Option<TaskRow>, ToolFailure> {
    let Some(target) = req.builds_on.filter(|_| req.kind.can_change_code()) else {
        return Ok(None);
    };
    let row = svc
        .db()
        .call(move |conn| task_store::load(conn, target))
        .await
        .map_err(|e| failed(&e))?;
    Ok(row.filter(|r| r.kind == TaskKind::Plan))
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
    let paths = svc.paths_of(&row);

    // 1. Folders.
    let mut dirs = vec![
        paths.root.clone(),
        paths.session.clone(),
        paths.artifacts.clone(),
        paths.evidence.clone(),
        paths.jobs.clone(),
        paths.skills.clone(),
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
    crate::reports::prepare(&paths, row.kind).map_err(|e| {
        PrepareError::Failed(format!("The report folder couldn't be prepared: {e}"))
    })?;
    svc.tell_outside(task, false);

    // 2. The project, then the workplace: a folder task works in its folder, which must still be
    // there; any other task gets a worktree on its branch.
    let project = svc
        .db()
        .call(move |conn| load_project(conn, row.project))
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?
        .ok_or_else(|| PrepareError::Failed(format!("{task}'s project is gone")))?;
    if row.delivery == DeliveryMode::Folder {
        check_folder(task, &workplace(svc, task).await?.dir).await?;
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
    let Some(settled) = recorded_base(
        project,
        row.base_branch.as_deref(),
        row.base_ref.as_deref(),
        row.base_commit.as_ref(),
    ) else {
        return Err(PrepareError::Failed(format!("{task} has no base recorded")));
    };
    let branch = settle_branch_name(svc, task, project, row, paths, &settled.commit).await?;
    let git = svc.deps().git.clone();
    let checkout = project.path.clone();
    let files = project.files_to_copy.clone();
    let wpaths = paths.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let repo = Repo::open(git, &checkout)?;
        repo.create_worktree(&branch, &wpaths, &settled, &files)
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

/// The base a task recorded, settled again from its parts: the typed branch (or the project's
/// default), the ref and the commit. `None` when no ref or commit was recorded.
pub(crate) fn recorded_base(
    project: &ProjectRow,
    base_branch: Option<&str>,
    base_ref: Option<&str>,
    base_commit: Option<&CommitSha>,
) -> Option<SettledBase> {
    let (Some(base_ref), Some(commit)) = (base_ref, base_commit) else {
        return None;
    };
    Some(SettledBase {
        source: if base_branch.is_some() {
            BaseSource::Typed
        } else {
            BaseSource::RemoteDefault
        },
        branch: base_branch.map_or_else(|| project.default_branch.clone(), str::to_owned),
        remote: project
            .github_remote
            .as_ref()
            .map(|_| qm_git::ORIGIN.to_owned()),
        base_ref: base_ref.to_owned(),
        commit: commit.clone(),
    })
}

/// Whether a new task of `kind` may start working in place in `folder` now, given every task open
/// in place. The outer `Result` is the store's; the inner one is the refusal.
pub(crate) fn check_writer_slot(
    conn: &Connection,
    in_place: bool,
    folder: &Path,
    kind: TaskKind,
) -> Result<Result<(), Refusal>, StoreError> {
    if !in_place {
        return Ok(Ok(()));
    }
    let open = task_store::open_in_place(conn)?;
    let open: Vec<(TaskId, &Path, TaskKind)> =
        open.iter().map(|(t, f, k)| (*t, f.as_path(), *k)).collect();
    Ok(check_folder_writer(in_place, folder, kind, &open))
}

/// [`check_writer_slot`] for `task` moving to work in place in `folder`: every other task open in
/// place counts, but not `task` itself.
pub(crate) fn check_writer_slot_excluding(
    conn: &Connection,
    task: TaskId,
    folder: &Path,
    kind: TaskKind,
) -> Result<Result<(), Refusal>, StoreError> {
    let open = task_store::open_in_place(conn)?;
    let open: Vec<(TaskId, &Path, TaskKind)> = open
        .iter()
        .filter(|(t, _, _)| *t != task)
        .map(|(t, f, k)| (*t, f.as_path(), *k))
        .collect();
    Ok(check_folder_writer(true, folder, kind, &open))
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
/// The branch the task's worktree goes on: the name recorded at start, unless since then a branch
/// of that name turned up in your checkout, on `origin`, or on GitHub. Then the next free name
/// from the title's list is recorded instead. Once the worktree exists its branch never changes.
/// If GitHub can't be asked, the local answer stands: the push would find the clash later.
async fn settle_branch_name(
    svc: &TaskService,
    task: TaskId,
    project: &ProjectRow,
    row: &TaskRow,
    paths: &TaskPaths,
    base: &CommitSha,
) -> Result<String, PrepareError> {
    let recorded = row.branch.clone();
    if recorded.is_empty() {
        return Err(PrepareError::Failed(format!(
            "{task} has no branch recorded"
        )));
    }
    if paths.worktree.exists() {
        return Ok(recorded);
    }
    let all = qm_policy::names::branch_candidates(&row.title);
    let candidates = match all.iter().position(|n| *n == recorded) {
        Some(at) => all[at..].to_vec(),
        None => vec![recorded.clone()],
    };
    for name in candidates {
        let git = svc.deps().git.clone();
        let checkout = project.path.clone();
        let local_name = name.clone();
        let start = base.clone();
        // A local branch at the base is an earlier attempt's, made but not yet used.
        let free_here = tokio::task::spawn_blocking(move || {
            let repo = Repo::open(git, &checkout)?;
            let local = repo.local_branch_commit(&local_name)?;
            let origin = repo.origin_branch_commit(&local_name)?;
            Ok::<_, GitError>(origin.is_none() && local.is_none_or(|tip| tip == start))
        })
        .await
        .map_err(|e| PrepareError::Failed(e.to_string()))?
        .map_err(|e| PrepareError::Failed(worktree_failure(task, &e)))?;
        if !free_here {
            continue;
        }
        let on_github = svc.deps().probe.remote_branch_taken(project, &name).await;
        if on_github == Some(true) {
            continue;
        }
        if name == recorded {
            return Ok(name);
        }
        let from = recorded.clone();
        let to = name.clone();
        let moved = svc
            .write(move |tx, _| {
                if task_store::first_free_branch(tx, std::slice::from_ref(&to))?.is_none() {
                    return Ok(false);
                }
                task_store::set_branch(tx, task, &from, &to)
            })
            .await
            .map_err(|e| PrepareError::Failed(e.to_string()))?;
        if moved {
            tracing::info!(%task, from = %recorded, to = %name, "branch name already taken; renamed");
            return Ok(name);
        }
    }
    Err(PrepareError::Failed(format!(
        "every branch name from {recorded} on is already taken; nothing was touched"
    )))
}

pub(crate) fn worktree_failure(task: TaskId, e: &GitError) -> String {
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
    // A retired project (its last task closed meanwhile) isn't listed by name any more: its folder
    // brings it back.
    let (folder, by_name) = if project.removed {
        (Some(project.path.display().to_string()), None)
    } else {
        (None, Some(project.name.clone()))
    };
    let req = StartRequest {
        folder,
        project: by_name,
        in_place: row.in_place && project.delivery != DeliveryMode::Folder,
        create_folder: false,
        label: None,
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
        preference: None,
        tier: None,
        effort: None,
        premium: false,
        choice_message: None,
        base_branch: None,
        verification,
        stronger_retry_of: Some(retried),
        correlation: None,
    };
    let how = StartAs {
        authority: Authority::CardChoice,
        retry_tier: Some(tier),
        user_words: Some(user_words),
        user_words_message: None,
        deferred: None,
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
            .map(|r| svc.plan_dir(&r)),
        None => None,
    };
    let mode = svc
        .live_plan_mode(row)
        .await
        .map_err(|e| PrepareError::Failed(format!("{e:?}")))?;
    let is_plan = row.kind == TaskKind::Plan;
    let dir = paths.plan.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if is_plan {
            crate::plans::create(&dir)?;
            if let Some(mode) = mode {
                crate::plans::prepare_template(&dir, mode)?;
            }
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

#[cfg(test)]
mod tier_tests {
    use super::*;

    fn route_store() -> (Connection, TaskId) {
        let mut conn = qm_store::connection::open_memory().unwrap();
        qm_store::schema::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, role, home, status, created_at, seq)
               VALUES (1, 'quartermaster', '/unused', 'idle', 0, 1);
             INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                 merge_method, created_at, seq)
               VALUES (1, 'p', '/unused', 'linked', 'main', 'direct_pr', 'merge', 0, 2);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                 delivery_mode, branch, status, created_at, seq)
               VALUES (1, 1, 1, 'lookup', 't', 'g', 'direct_pr', 'qm/task-1', 'queued', 0, 3);",
        )
        .unwrap();
        (conn, TaskId::new(1).unwrap())
    }

    #[test]
    fn started_event_persists_tier_for_reroute_and_reads_legacy_without_tier() {
        let (conn, task) = route_store();
        let inputs = RouteInputs {
            named_tier: Some(Tier::Medium),
            preference: Some(qm_types::model_preference::ModelPreference {
                selection: ModelId::new("preferred").unwrap(),
                provider: None,
                model: None,
                effort: Some(Effort::High),
            }),
            named_effort: Some(Effort::High),
            authority: "user".into(),
            message: Some(MessageId::new(1)),
            ..RouteInputs::default()
        };
        let at = "2026-10-23T00:00:00Z".parse().unwrap();
        for legacy in [false, true] {
            let mut route = serde_json::to_value(&inputs).unwrap();
            if legacy {
                route.as_object_mut().unwrap().remove("named_tier");
            }
            task_store::event(
                &conn,
                STARTED_EVENT,
                task,
                None,
                "task started",
                serde_json::json!({ "route": route }),
                at,
            )
            .unwrap();
            let restored = route_inputs(&conn, task).unwrap();
            let expected = RouteInputs {
                named_tier: if legacy { None } else { Some(Tier::Medium) },
                ..inputs.clone()
            };
            assert_eq!(restored, expected);
            let facts = facts_for(task, None, TaskKind::Lookup, &restored, false);
            assert_eq!(facts.named_tier, expected.named_tier);
            assert_eq!(facts.preference, expected.preference);
            assert_eq!(facts.named_effort, Some(Effort::High));
            assert_eq!(facts.authority, inputs.authority());
        }
    }

    #[test]
    fn persisted_tier_round_trips_into_reroute_facts_and_old_inputs_default_to_none() {
        let inputs = RouteInputs {
            named_tier: Some(Tier::Medium),
            authority: "user".into(),
            message: Some(MessageId::new(1)),
            ..RouteInputs::default()
        };
        let value = serde_json::to_value(&inputs).unwrap();
        assert_eq!(value["named_tier"], "medium");
        let restored: RouteInputs = serde_json::from_value(value).unwrap();
        let facts = facts_for(
            TaskId::new(1).unwrap(),
            None,
            TaskKind::Lookup,
            &restored,
            false,
        );
        assert_eq!(facts.named_tier, Some(Tier::Medium));
        assert_eq!(facts.named_effort, None);

        let mut legacy = serde_json::to_value(&inputs).unwrap();
        legacy.as_object_mut().unwrap().remove("named_tier");
        let restored: RouteInputs = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.named_tier, None);
        assert!(
            serde_json::to_value(restored)
                .unwrap()
                .get("named_tier")
                .is_none()
        );
    }
}
