//! Delivery: pull requests, CI polling, the delivery probe, the GitHub tools and the fence's app
//! side.
//!
//! - [`pr`]: `pr_open` and `pr_status`.
//! - [`checks`]: the CI poller ("passed" means every check on the exact commit), `ci_results`,
//!   reruns.
//! - [`probe`]: [`probe::GithubDeliveryProbe`], what `delivery_check` and the start gate share.
//! - [`github_tools`]: `github_read` and `github_write` (the write fence).
//! - `push_guard`: every push the app makes, only to the project's repository, confirmed on
//!   GitHub before it's recorded.
//! - [`ports`]: what delivery needs from verification and secondmates, with safe defaults.
//! - [`excerpts`]: outside text neutralised and cut on its way to an agent.
//!
//! Every push and every GitHub call runs in the app's own process with its own token.

pub mod checks;
pub mod excerpts;
pub mod github_tools;
pub mod ports;
pub mod pr;
pub mod probe;
pub(crate) mod push_guard;

use crate::scheduler::Scheduler;
use crate::tasks::close::CloseFacts;
use crate::tasks::{ProjectRow, TaskService, failed, refuse};
use ports::{ParentPort, VerificationPort};
use qm_git::Git;
use qm_github::{GithubClient, RepoRef};
use qm_store::StoreError;
use qm_store::delivery as delivery_store;
use qm_store::reports::ReportRow;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::{CiReport, GithubContent, GithubReadArgs, GithubWriteArgs};
use qm_tools::defs::{GithubWriteOutcome, PrOpened, PrStatus};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::{DeliveryMode, TaskStatus};
use qm_types::ids::{ModelId, TaskId};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::OwnedMutexGuard;

/// `events.kind` of a pull request closed because its task ended another way.
pub const PR_CLOSED_EVENT: &str = "pr.closed";

/// Close a direct-PR task's open pull request when the task is stopped, discarded, failed or
/// retried.
pub const CLOSE_PR_ON_SIDE_EXIT: bool = true;

/// One in-memory lock per task, shared by `pr_open` and settling: a push and a merge of the same
/// task never interleave.
#[derive(Clone, Default)]
pub struct TaskLocks(Arc<Mutex<HashMap<TaskId, Arc<tokio::sync::Mutex<()>>>>>);

impl fmt::Debug for TaskLocks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskLocks").finish_non_exhaustive()
    }
}

impl TaskLocks {
    /// Waits for and holds `task`'s lock.
    pub async fn lock(&self, task: TaskId) -> OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(map.entry(task).or_default())
        };
        lock.lock_owned().await
    }
}

/// What the delivery service is built from.
pub struct DeliveryDeps {
    /// The task service; its git and scheduler are delivery's too.
    pub tasks: TaskService,
    pub github: GithubClient,
    /// The `gh` the app pushes through for `https://` remotes (`gh auth git-credential`),
    /// read at each push: one installed from the setup check is used without a restart.
    pub gh: qm_github::GhProgram,
    pub verification: Arc<dyn VerificationPort>,
    pub parent: Arc<dyn ParentPort>,
}

impl fmt::Debug for DeliveryDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeliveryDeps")
            .field("github", &self.github)
            .finish_non_exhaustive()
    }
}

/// Pull requests, checks and the GitHub tools. Cheap to clone.
#[derive(Clone, Debug)]
pub struct DeliveryService {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    deps: DeliveryDeps,
    locks: TaskLocks,
}

impl DeliveryService {
    pub fn new(deps: DeliveryDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                locks: TaskLocks::default(),
            }),
        }
    }

    pub(crate) fn deps(&self) -> &DeliveryDeps {
        &self.inner.deps
    }

    pub(crate) fn tasks(&self) -> &TaskService {
        &self.inner.deps.tasks
    }

    pub(crate) fn git(&self) -> Git {
        self.inner.deps.tasks.deps().git.clone()
    }

    pub(crate) fn scheduler(&self) -> &Scheduler {
        &self.inner.deps.tasks.deps().scheduler
    }

    pub(crate) fn github(&self) -> &GithubClient {
        &self.inner.deps.github
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        self.inner.deps.tasks.clock()
    }

    /// The per-task locks settling shares.
    pub fn locks(&self) -> &TaskLocks {
        &self.inner.locks
    }

    pub(crate) fn verification(&self) -> Arc<dyn VerificationPort> {
        Arc::clone(&self.inner.deps.verification)
    }

    pub(crate) fn parent(&self) -> Arc<dyn ParentPort> {
        Arc::clone(&self.inner.deps.parent)
    }

    /// Runs `f` on the database thread.
    pub(crate) async fn read<R, F>(&self, f: F) -> Result<R, ToolFailure>
    where
        F: FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        self.tasks().db().call(f).await.map_err(|e| failed(&e))
    }

    /// The task, its project and its delivery columns.
    pub(crate) async fn load(&self, task: TaskId) -> Result<Loaded, ToolFailure> {
        self.read(move |conn| load_facts(conn, task))
            .await?
            .ok_or_else(|| refuse(format!("{task} doesn't exist")))
    }

    // ---------------------------------------------------------------------------------------
    // The tool host's methods
    // ---------------------------------------------------------------------------------------

    pub async fn open_pull_request(
        &self,
        caller: &Caller,
        title: String,
        body: String,
    ) -> Result<PrOpened, ToolFailure> {
        pr::open(self, caller, title, body).await
    }

    pub async fn pull_request_status(&self, caller: &Caller) -> Result<PrStatus, ToolFailure> {
        pr::status(self, caller).await
    }

    pub async fn ci_results(
        &self,
        caller: &Caller,
        check: Option<String>,
    ) -> Result<CiReport, ToolFailure> {
        checks::ci_results(self, caller, check).await
    }

    pub async fn github_read(
        &self,
        caller: &Caller,
        args: GithubReadArgs,
    ) -> Result<GithubContent, ToolFailure> {
        github_tools::read(self, caller, args).await
    }

    pub async fn github_write(
        &self,
        caller: &Caller,
        args: GithubWriteArgs,
    ) -> Result<GithubWriteOutcome, ToolFailure> {
        github_tools::write(self, caller, args).await
    }

    /// Reruns one failing check on the task's pushed head and re-arms the poller (verification's
    /// `CiPort::rerun` calls this after its own rule allows it).
    pub async fn rerun_check(&self, task: TaskId, check: &str) -> Result<(), ToolFailure> {
        checks::rerun(self, task, check).await
    }

    /// Startup: re-arms the poller for every pushed head whose checks aren't final yet.
    pub async fn recover(&self) -> Result<(), StoreError> {
        checks::recover(self).await
    }
}

/// A task with its project and delivery facts.
#[derive(Clone, Debug)]
pub(crate) struct Loaded {
    pub row: TaskRow,
    pub project: ProjectRow,
    pub delivery: delivery_store::DeliveryRow,
}

impl Loaded {
    /// The project's GitHub repository, if its remote is on GitHub.
    pub fn repo(&self) -> Option<RepoRef> {
        self.project
            .github_remote
            .as_deref()
            .and_then(RepoRef::from_project)
    }

    /// The branch the task's work targets: its base branch, else the project's default.
    pub fn target(&self) -> String {
        self.row
            .base_branch
            .clone()
            .unwrap_or_else(|| self.project.default_branch.clone())
    }
}

pub(crate) fn load_facts(conn: &Connection, task: TaskId) -> Result<Option<Loaded>, StoreError> {
    let Some(row) = task_store::load(conn, task)? else {
        return Ok(None);
    };
    let Some(project) = qm_store::projects::load(conn, row.project)? else {
        return Ok(None);
    };
    let delivery = delivery_store::load(conn, task)?.unwrap_or_default();
    Ok(Some(Loaded {
        row,
        project,
        delivery,
    }))
}

/// The pool's model IDs, refused in a `Co-Authored-By` trailer.
pub(crate) fn models(conn: &Connection) -> Result<Vec<ModelId>, StoreError> {
    Ok(qm_store::models::pool(conn)?
        .into_iter()
        .flat_map(|p| [p.facts.model, p.facts.canonical])
        .collect())
}

/// The caller's own task: only a crewmate has one.
pub(crate) fn callers_task(caller: &Caller) -> Result<TaskId, ToolFailure> {
    caller
        .task
        .ok_or_else(|| refuse("only a crewmate's task has a branch and a pull request"))
}

/// The five headings of a PR body: each at the start of a line, optionally after `#`s or `**`,
/// case-insensitive. Returns the missing ones (Verified also when it's blank).
pub(crate) fn missing_body_headings(body: &str) -> Vec<&'static str> {
    const HEADINGS: [&str; 5] = ["Intent", "What changed", "Verified", "Risk", "Not done"];
    let lines: Vec<String> = body
        .lines()
        .map(|l| {
            l.trim_start()
                .trim_start_matches('#')
                .trim_start()
                .trim_start_matches("**")
                .trim_start()
                .to_lowercase()
        })
        .collect();
    let starts = |line: &str, heading: &str| {
        let h = heading.to_lowercase();
        line.strip_prefix(&h).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with(':')
                || rest.starts_with("**")
                || rest.starts_with(' ')
        })
    };
    let mut missing = Vec::new();
    for heading in HEADINGS {
        let Some(i) = lines.iter().position(|l| starts(l, heading)) else {
            missing.push(heading);
            continue;
        };
        if heading == "Verified" {
            // Not blank: something after the heading on its line, or before the next heading.
            let own = lines[i]
                .split_once(':')
                .map(|(_, rest)| rest.trim().trim_start_matches("**").trim().to_owned())
                .unwrap_or_default();
            let next = lines[i + 1..]
                .iter()
                .take_while(|l| !HEADINGS.iter().any(|h| starts(l, h)))
                .any(|l| !l.trim().is_empty());
            if own.is_empty() && !next {
                missing.push(heading);
            }
        }
    }
    missing
}

/// Whether a done report's Verified heading names something (the local-checks rule).
pub(crate) fn verified_named(report: &ReportRow) -> bool {
    report
        .headings
        .get("verified")
        .and_then(|v| v.as_str())
        .is_some_and(|v| !v.trim().is_empty())
}

/// Whether a done report carries all five headings.
pub(crate) fn report_complete(report: &ReportRow) -> bool {
    ["intent", "what_changed", "verified", "risk", "not_done"]
        .iter()
        .all(|k| {
            report
                .headings
                .get(*k)
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.trim().is_empty())
        })
}

/// Delivery's `CloseFacts` hook for a task's retirement: where landed work went, and closing an
/// open pull request when a task ends another way.
#[derive(Clone, Debug)]
pub struct DeliveryCloseFacts {
    delivery: DeliveryService,
}

impl DeliveryCloseFacts {
    pub fn new(delivery: DeliveryService) -> Self {
        Self { delivery }
    }
}

impl CloseFacts for DeliveryCloseFacts {
    fn merged_into(&self, conn: &Connection, task: TaskId) -> Vec<String> {
        match delivery_store::landed_into(conn, task) {
            Ok(into) => into.into_iter().collect(),
            Err(e) => {
                tracing::warn!(%task, error = %e, "couldn't read where the task landed");
                Vec::new()
            }
        }
    }

    fn on_side_exit(&self, task: TaskId) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !CLOSE_PR_ON_SIDE_EXIT {
                return;
            }
            let Ok(loaded) = self.delivery.load(task).await else {
                return;
            };
            let reason = match loaded.row.status {
                TaskStatus::Stopped => "the task was stopped",
                TaskStatus::Discarded => "the task was discarded",
                TaskStatus::Failed => "the task failed",
                _ => return,
            };
            let (Some(pr), Some(repo), DeliveryMode::DirectPr) = (
                loaded.delivery.pr_number,
                loaded.repo(),
                loaded.row.delivery,
            ) else {
                return;
            };
            let github = self.delivery.github().clone();
            match github.pull(&repo, pr).await {
                Ok(pull) if pull.state == qm_github::PrState::Open => {}
                _ => return,
            }
            let comment = format!("Closed by Quartermaster: {reason}.");
            let summary = match github.close_pr(&repo, pr, &comment).await {
                Ok(()) => format!("PR #{pr} closed: {reason}"),
                Err(e) => format!("PR #{pr} couldn't be closed: {}", e.plain()),
            };
            let at = self.delivery.clock().now();
            let _ = self
                .delivery
                .tasks()
                .write(move |tx, _| {
                    task_store::event(
                        tx,
                        PR_CLOSED_EVENT,
                        task,
                        None,
                        &summary,
                        serde_json::json!({ "pr": pr }),
                        at,
                    )
                    .map(drop)
                })
                .await;
        })
    }
}
