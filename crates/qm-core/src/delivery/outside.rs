//! What happened on GitHub outside the app. Crewmates have the user's GitHub login, so a pull
//! request can be merged, or a tag pushed, without the app doing it. The app records each such
//! fact on the task and tells the supervisor once; it never pushes, moves or deletes anything in
//! answer.
//!
//! - **A merged pull request.** The task's pull request is read at report, at `settle_task`, and
//!   every [`WATCH_EVERY`] while it is open ([`WATCH_SLOW`] when GitHub's rate limit runs low). If
//!   GitHub shows it merged and the app has no landing of its own, the task records a
//!   [`LANDED_OUTSIDE_EVENT`] and lands (`settling::facts::landed_outside`), and its supervisor
//!   gets one `landed_outside` wake.
//! - **A new tag.** When a task starts on a GitHub project, the repository's tag names are kept in
//!   `tasks/C-n/tags-at-start` ([`TAGS_FILE`]). At report and at settle they are listed again;
//!   each new one the app wasn't asked to push (`propose_release_tag`) gets a
//!   [`RELEASED_OUTSIDE_EVENT`] and one `released_outside` wake. A task with no baseline (started
//!   before this existed) skips the tag check.

use super::DeliveryService;
use super::checks::RATE_LOW;
use crate::inbox::Inbox;
use crate::settling::facts::{self, Fact};
use crate::tasks::lifecycle::{TaskEvent, is_open, next_for};
use crate::tasks::new_wake;
use qm_git::Repo;
use qm_store::StoreError;
use qm_store::delivery as delivery_store;
use qm_store::releases;
use qm_store::tasks as task_store;
use qm_types::domain::{DeliveryMode, TaskStatus, WakeKind};
use qm_types::ids::{CommitSha, TaskId};
use std::collections::BTreeSet;
use std::time::Duration;

/// `events.kind` of a pull request merged outside the app.
pub const LANDED_OUTSIDE_EVENT: &str = "landed_outside";
/// `events.kind` of a tag the app didn't push.
pub const RELEASED_OUTSIDE_EVENT: &str = "released_outside";
/// How often an open pull request is read.
pub const WATCH_EVERY: Duration = Duration::from_mins(5);
/// The interval when GitHub's rate limit runs low.
pub const WATCH_SLOW: Duration = Duration::from_mins(15);
/// The tag baseline's file in the task folder: one tag name per line.
pub const TAGS_FILE: &str = "tags-at-start";
/// The most new tags recorded at one check.
const MAX_NEW_TAGS: usize = 10;

/// What the pull-request check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PrSeen {
    /// Nothing to watch: no pull request, not a GitHub project, or the task has moved on.
    Nothing,
    /// Open (or closed unmerged): keep watching while the task is open.
    Open,
    /// Merged outside the app, and recorded now or before.
    MergedOutside { pr: u64, merge_commit: CommitSha },
}

fn key(task: TaskId) -> String {
    format!("pr-watch:{task}")
}

/// Reads the task's pull request after `delay`, replacing any read already armed.
pub(crate) fn arm(svc: &DeliveryService, task: TaskId, delay: Duration) {
    let owned = svc.clone();
    svc.scheduler().after(
        key(task),
        delay,
        Box::new(move || Box::pin(async move { watch_once(&owned, task).await })),
    );
}

async fn watch_once(svc: &DeliveryService, task: TaskId) {
    let seen = {
        let _lock = svc.locks().lock(task).await;
        check_pr_locked(svc, task).await
    };
    match seen {
        Ok(PrSeen::Open) => {
            let low = svc.github().rate().remaining.is_some_and(|l| l < RATE_LOW);
            arm(svc, task, if low { WATCH_SLOW } else { WATCH_EVERY });
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(%task, error = %e, "the pull-request watch couldn't read GitHub");
            arm(svc, task, WATCH_SLOW);
        }
    }
}

/// Startup: arms the watch for every open task with a pull request.
pub(crate) async fn recover(svc: &DeliveryService) -> Result<(), StoreError> {
    let tasks = svc
        .tasks()
        .db()
        .call(|conn| delivery_store::with_pull_request(conn))
        .await?;
    for task in tasks {
        arm(svc, task, WATCH_EVERY);
    }
    Ok(())
}

/// The pull-request check, at report: takes the task's lock.
pub(crate) async fn check_pr(svc: &DeliveryService, task: TaskId) -> Result<PrSeen, String> {
    let _lock = svc.locks().lock(task).await;
    check_pr_locked(svc, task).await
}

/// The pull-request check, with the task's lock already held (settling holds it).
pub(crate) async fn check_pr_locked(svc: &DeliveryService, task: TaskId) -> Result<PrSeen, String> {
    let loaded = svc.load(task).await.map_err(|e| format!("{e:?}"))?;
    let status = loaded.row.status;
    if !is_open(status) || matches!(status, TaskStatus::Settling | TaskStatus::Landed) {
        return Ok(PrSeen::Nothing);
    }
    let (Some(pr), Some(repo), DeliveryMode::DirectPr) = (
        loaded.delivery.pr_number,
        loaded.repo(),
        loaded.row.delivery,
    ) else {
        return Ok(PrSeen::Nothing);
    };
    let pull = svc.github().pull(&repo, pr).await.map_err(|e| e.plain())?;
    let (true, Some(merge_commit)) = (pull.merged, pull.merge_commit.clone()) else {
        return Ok(PrSeen::Open);
    };
    let fact = Fact::MergedPr {
        pr,
        head: pull.head_sha.clone(),
        merge_commit: merge_commit.clone(),
        base: pull.base_ref.clone(),
    };
    let merged_by = pull.merged_by.clone();
    let parent = svc.parent();
    let at = svc.clock().now();
    let payload_commit = merge_commit.as_str().to_owned();
    svc.tasks()
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(());
            };
            if task_store::last_event(tx, task, LANDED_OUTSIDE_EVENT)?.is_some() {
                return Ok(());
            }
            let by = merged_by.as_deref();
            task_store::event(
                tx,
                LANDED_OUTSIDE_EVENT,
                task,
                None,
                &format!(
                    "PR #{pr} merged outside the app{}",
                    by.map(|b| format!(" by {b}")).unwrap_or_default()
                ),
                serde_json::json!({
                    "pr": pr, "merge_commit": payload_commit, "merged_by": by,
                }),
                at,
            )?;
            // Reported or running: it lands like any merge. Otherwise (held, stuck, waiting) the
            // status stays, and the supervisor is told; a later settle finds it merged.
            if let Ok(to) = next_for(task, row.status, TaskEvent::SettleStarted) {
                task_store::set_status(tx, task, row.status, to, at)?;
                facts::landed_outside(tx, notify, &*parent, task, &fact, by, at)?;
            } else {
                Inbox::post(
                    tx,
                    &new_wake(
                        row.supervisor,
                        WakeKind::LandedOutside,
                        task,
                        format!("landed_outside:{task}"),
                        &format!(
                            "{task}'s PR #{pr} was merged on GitHub outside the app{} while the \
                             task is {}. Tell the user in a reply, and close the task when its \
                             crewmate is done.",
                            by.map(|b| format!(" by {b}")).unwrap_or_default(),
                            row.status
                        ),
                        &format!("{task} PR #{pr} merged outside the app"),
                        serde_json::json!({ "pr": pr }),
                        at,
                    ),
                    notify,
                )?;
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(PrSeen::MergedOutside { pr, merge_commit })
}

/// The repository's tag names now, through the project's checkout and the app's own transport.
async fn remote_tags(svc: &DeliveryService, task: TaskId) -> Result<Option<Vec<String>>, String> {
    let loaded = svc.load(task).await.map_err(|e| format!("{e:?}"))?;
    if loaded.repo().is_none() {
        return Ok(None);
    }
    let git = svc.git();
    let checkout = loaded.project.path.clone();
    tokio::task::spawn_blocking(move || {
        let repo = Repo::open(git, &checkout).map_err(|e| e.to_string())?;
        repo.remote_tags("origin").map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
    .map(Some)
}

/// At a task's start on a GitHub project: keeps the repository's tag names, once.
pub(crate) async fn record_baseline(svc: &DeliveryService, task: TaskId) {
    let file = svc
        .tasks()
        .deps()
        .data_dir
        .crewmate_root(task)
        .join(TAGS_FILE);
    if file.exists() {
        return;
    }
    match remote_tags(svc, task).await {
        Ok(Some(tags)) => {
            let mut text = tags.join("\n");
            text.push('\n');
            if let Err(e) = std::fs::write(&file, text) {
                tracing::warn!(%task, error = %e, "couldn't keep the tag baseline");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(%task, error = %e, "couldn't list the tags at start"),
    }
}

/// At report and at settle: records each tag that appeared since the task started and that the
/// app wasn't asked to push. Returns the tags recorded now.
pub(crate) async fn check_tags(svc: &DeliveryService, task: TaskId) -> Result<Vec<String>, String> {
    let file = svc
        .tasks()
        .deps()
        .data_dir
        .crewmate_root(task)
        .join(TAGS_FILE);
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(Vec::new());
    };
    let Some(now) = remote_tags(svc, task).await? else {
        return Ok(Vec::new());
    };
    let fresh = since_baseline(&text, now);
    if fresh.is_empty() {
        return Ok(Vec::new());
    }
    let at = svc.clock().now();
    svc.tasks()
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(Vec::new());
            };
            let ours: BTreeSet<String> = releases::tags_of_project(tx, row.project)?
                .into_iter()
                .collect();
            let seen: BTreeSet<String> =
                task_store::event_payloads(tx, task, RELEASED_OUTSIDE_EVENT)?
                    .into_iter()
                    .filter_map(|p| p.get("tag")?.as_str().map(str::to_owned))
                    .collect();
            let mut recorded = Vec::new();
            for tag in to_record(fresh, &ours, &seen) {
                task_store::event(
                    tx,
                    RELEASED_OUTSIDE_EVENT,
                    task,
                    None,
                    &format!("tag {tag} appeared outside the app"),
                    serde_json::json!({ "tag": tag }),
                    at,
                )?;
                Inbox::post(
                    tx,
                    &new_wake(
                        row.supervisor,
                        WakeKind::ReleasedOutside,
                        task,
                        format!("released_outside:{task}:{tag}"),
                        &format!(
                            "The tag {tag} appeared on {task}'s repository since the task started, \
                             and the app didn't push it. Tell the user in a reply."
                        ),
                        &format!("tag {tag} pushed outside the app"),
                        serde_json::json!({ "tag": tag }),
                        at,
                    ),
                    notify,
                )?;
                recorded.push(tag);
            }
            Ok(recorded)
        })
        .await
        .map_err(|e| e.to_string())
}

/// The tags in `now` that the baseline file's lines don't name.
fn since_baseline(baseline: &str, now: Vec<String>) -> Vec<String> {
    let before: BTreeSet<&str> = baseline.lines().filter(|l| !l.is_empty()).collect();
    now.into_iter()
        .filter(|t| !before.contains(t.as_str()))
        .collect()
}

/// The new tags worth recording: not pushed by the app, not recorded before, at most
/// [`MAX_NEW_TAGS`].
fn to_record(fresh: Vec<String>, ours: &BTreeSet<String>, seen: &BTreeSet<String>) -> Vec<String> {
    fresh
        .into_iter()
        .filter(|t| !ours.contains(t) && !seen.contains(t))
        .take(MAX_NEW_TAGS)
        .collect()
}

/// Delivery's hook on a task's start and report: the tag baseline, then the checks.
#[derive(Clone, Debug)]
pub struct OutsideHook {
    delivery: DeliveryService,
}

impl OutsideHook {
    pub fn new(delivery: DeliveryService) -> Self {
        Self { delivery }
    }
}

impl crate::tasks::OutsideWatch for OutsideHook {
    fn started(&self, task: TaskId) -> qm_types::BoxFuture<'_, ()> {
        Box::pin(async move { record_baseline(&self.delivery, task).await })
    }

    fn reported(&self, task: TaskId) -> qm_types::BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(e) = check_pr(&self.delivery, task).await {
                tracing::warn!(%task, error = %e, "couldn't read the pull request at report");
            }
            if let Err(e) = check_tags(&self.delivery, task).await {
                tracing::warn!(%task, error = %e, "couldn't list the tags at report");
            }
        })
    }
}
