//! Merging a pull request at exactly the approved commit. Merging the wrong commit is the worst
//! failure the product can have, so every fact is read again from GitHub right before the merge,
//! the merge carries the SHA as its precondition, and nothing counts until GitHub says so twice.

use super::facts::{self, Fact};
use super::recover::{MergeWatch, RETRY_EVERY, watch_after};
use super::review::review_refusal;
use super::{
    Landing, MERGE_REQUESTED_EVENT, MERGE_UNKNOWN_EVENT, Origin, SettleError, SettlingService,
};
use crate::delivery::Loaded;
use crate::delivery::checks::{Reading, local_checks_named, read_snapshot};
use qm_github::{GithubClient, MergeResult, PrState, PullRequest, RepoRef};
use qm_policy::merge_gate::{check_approval, check_mergeable};
use qm_store::delivery::ChecksState;
use qm_store::held::MergePayload;
use qm_store::tasks as task_store;
use qm_types::domain::{MergeMethod, TaskStatus};
use qm_types::ids::{CommitSha, TaskId};
use std::time::Duration;

/// The read-back's waits after GitHub says merged (2 s, 4 s, 8 s …, at most 2 minutes in all).
const READ_BACK: [Duration; 6] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
    Duration::from_secs(58),
];
/// The wait before the one retry of an ambiguous merge whose PR is still open at the approved head.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// The pull request merge, exactly as stored or as autonomy built it.
#[derive(Clone, Debug)]
struct PrMerge<'a> {
    task: TaskId,
    repo: RepoRef,
    pr: u64,
    head: &'a CommitSha,
    base: &'a str,
    method: MergeMethod,
}

/// Merges `payload` (a `MergePayload::PullRequest`) for `origin`. A refusal is already recorded
/// when this returns `Landing::Refused`.
pub async fn land_pr(
    svc: &SettlingService,
    payload: &MergePayload,
    origin: Origin,
) -> Result<Landing, SettleError> {
    // Only the payload's own fields are used from here on.
    let MergePayload::PullRequest {
        task,
        repo,
        pr,
        head,
        base,
        method,
    } = payload
    else {
        return Err(SettleError::Failed(
            "land_pr was given a merge that isn't a pull request".to_owned(),
        ));
    };
    let Some(repo_ref) = RepoRef::parse(repo) else {
        return refused(
            svc,
            *task,
            head,
            format!("{repo:?} isn't owner/name"),
            false,
            origin,
        )
        .await;
    };
    let p = PrMerge {
        task: *task,
        repo: repo_ref,
        pr: *pr,
        head,
        base,
        method: *method,
    };
    // One settle of a task at a time.
    let _lock = svc.delivery().locks().lock(p.task).await;

    match verify(svc, &p).await? {
        Verified::Go => {}
        Verified::AlreadyMerged(merge_commit) => {
            return land(svc, &p, merge_commit).await;
        }
        Verified::No { reason, superseded } => {
            return refused(svc, p.task, p.head, reason, superseded, origin).await;
        }
    }

    // The request is on record before GitHub hears of it.
    let at = svc.delivery().clock().now();
    let held = match origin {
        Origin::Approval { held } => Some(held),
        Origin::Autonomy => None,
    };
    let event_payload = serde_json::json!({
        "repo": p.repo.to_string(), "pr": p.pr, "head": p.head, "base": p.base, "held": held,
    });
    let task = p.task;
    let summary = format!(
        "{task}: merge of PR #{} at {} requested",
        p.pr,
        p.head.short()
    );
    let requested = svc
        .tasks()
        .write(move |tx, _| {
            task_store::event(
                tx,
                MERGE_REQUESTED_EVENT,
                task,
                None,
                &summary,
                event_payload,
                at,
            )
        })
        .await?;
    let watch = MergeWatch {
        requested,
        requested_at: at,
        repo: p.repo.to_string(),
        pr: p.pr,
        head: p.head.clone(),
        base: p.base.to_owned(),
        held,
    };

    let github = svc.delivery().github().clone();
    // The merge carries the approved SHA as its precondition.
    let first = github.merge_pr(&p.repo, p.pr, p.head, p.method).await;
    match conclude(svc, &github, &p, first, true).await {
        Concluded::Merged(merge_commit) => land(svc, &p, merge_commit).await,
        Concluded::No { reason, superseded } => {
            refused(svc, p.task, p.head, reason, superseded, origin).await
        }
        Concluded::Unknown(reason) => unknown(svc, &p, watch, reason).await,
    }
}

/// GitHub answered late or unclearly. Nothing counts yet either way: the task stays settling, the
/// fact is on record, and the merge is read back until GitHub confirms it (`recover::watch_after`).
async fn unknown(
    svc: &SettlingService,
    p: &PrMerge<'_>,
    watch: MergeWatch,
    reason: String,
) -> Result<Landing, SettleError> {
    let at = svc.delivery().clock().now();
    let task = p.task;
    let summary = format!(
        "{task}: the merge of PR #{} at {} isn't confirmed yet: {reason}",
        p.pr,
        p.head.short()
    );
    let payload = serde_json::json!({
        "pr": p.pr, "head": p.head, "reason": reason, "requested": watch.requested,
    });
    svc.tasks()
        .write(move |tx, _| {
            task_store::event(tx, MERGE_UNKNOWN_EVENT, task, None, &summary, payload, at).map(drop)
        })
        .await?;
    watch_after(svc, task, watch, RETRY_EVERY);
    Ok(Landing::Unknown { reason })
}

/// GitHub must report at least as many checks on this head as the poller saw on it. A check that
/// vanished from the answer isn't a pass, and a head whose checks all vanished isn't a project
/// without CI (green only on another commit merges nothing).
pub(crate) fn vanished_checks(
    reading: &Reading,
    seen: Option<&ChecksState>,
    head: &CommitSha,
) -> Option<String> {
    let seen = seen.filter(|s| &s.head == head)?;
    (reading.total < seen.total).then(|| {
        let checks = |n: u32| {
            if n == 1 {
                "1 check".to_owned()
            } else {
                format!("{n} checks")
            }
        };
        format!(
            "GitHub now reports {} on {} where {} {} seen",
            checks(reading.total),
            head.short(),
            checks(seen.total),
            if seen.total == 1 { "was" } else { "were" }
        )
    })
}

/// When the branch's required checks couldn't be read, only GitHub's own "clean" with every
/// reported check passing counts.
pub(crate) fn unreadable_required(
    complete: bool,
    mergeable_state: Option<&str>,
    reading: &Reading,
) -> Option<String> {
    if complete {
        return None;
    }
    if mergeable_state != Some("clean") {
        return Some(
            "the branch's required checks couldn't be read and GitHub doesn't report the pull request as cleanly mergeable".to_owned(),
        );
    }
    if reading.total > 0 && reading.passed < reading.total {
        return Some(format!(
            "the branch's required checks couldn't be read, so every reported check must pass, and {} of {} did",
            reading.passed, reading.total
        ));
    }
    None
}

enum Verified {
    Go,
    AlreadyMerged(CommitSha),
    No { reason: String, superseded: bool },
}

fn no(reason: impl Into<String>) -> Verified {
    Verified::No {
        reason: reason.into(),
        superseded: false,
    }
}

/// Everything again, from GitHub and the task's current facts, on exactly the approved commit.
#[allow(clippy::too_many_lines)]
async fn verify(svc: &SettlingService, p: &PrMerge<'_>) -> Result<Verified, SettleError> {
    let task = p.task;
    let loaded: Loaded = match svc.delivery().load(task).await {
        Ok(l) => l,
        Err(e) => return Ok(no(e.render())),
    };
    if loaded.row.status != TaskStatus::Settling {
        return Ok(no(format!("{task} is {}, not settling", loaded.row.status)));
    }
    // The task's pushed head is the approved head.
    match &loaded.delivery.pushed_head {
        Some(pushed) if pushed == p.head => {}
        Some(pushed) => {
            return Ok(Verified::No {
                reason: moved(p.head, pushed),
                superseded: true,
            });
        }
        None => {
            return Ok(no(format!(
                "{} isn't a commit the app pushed",
                p.head.short()
            )));
        }
    }
    let github = svc.delivery().github();
    let pull = match github.pull(&p.repo, p.pr).await {
        Ok(pull) => pull,
        Err(e) => return Ok(no(format!("GitHub couldn't be read: {}", e.plain()))),
    };
    // The PR is the approved one, open and unmerged.
    if pull.number != p.pr {
        return Ok(no(format!(
            "GitHub answered with PR #{}, not #{}",
            pull.number, p.pr
        )));
    }
    if pull.merged {
        if &pull.head_sha == p.head
            && let Some(merge_commit) = pull.merge_commit.clone()
        {
            return Ok(Verified::AlreadyMerged(merge_commit));
        }
        return Ok(no(format!(
            "PR #{} was already merged at another commit",
            p.pr
        )));
    }
    if pull.state != PrState::Open {
        return Ok(no(format!("PR #{} is closed", p.pr)));
    }
    // The PR's head is exactly the approved SHA.
    if &pull.head_sha != p.head {
        return Ok(Verified::No {
            reason: moved(p.head, &pull.head_sha),
            superseded: true,
        });
    }
    // The base and the head's repository are the approved ones (no fork).
    if pull.base_ref != p.base {
        return Ok(no(format!(
            "PR #{} now targets {}, not {}",
            p.pr, pull.base_ref, p.base
        )));
    }
    if pull.head_repo.as_ref() != Some(&p.repo) {
        return Ok(no(format!(
            "PR #{}'s branch isn't in {} any more",
            p.pr, p.repo
        )));
    }
    // The check runs and commit statuses fetched by that SHA pass, with every required check
    // present.
    let snapshot = match github.checks(&p.repo, p.head, p.base).await {
        Ok(s) => s,
        Err(e) => return Ok(no(format!("the checks couldn't be read: {}", e.plain()))),
    };
    let reading = read_snapshot(&snapshot);
    let head = p.head.clone();
    let port = svc.delivery().verification();
    let (local_named, gate, stale) = svc
        .tasks()
        .db()
        .call(move |conn| {
            let gate = port.gate(conn, task)?;
            Ok((
                local_checks_named(conn, task, &head)?,
                gate,
                review_refusal(conn, task, gate, &head)?,
            ))
        })
        .await?;
    if let Err(r) = check_approval(p.head, &pull.head_sha, &reading.verdict, local_named) {
        return Ok(no(r.reason));
    }
    // Checks seen on this head that vanished don't count as none.
    if let Some(reason) = vanished_checks(&reading, loaded.delivery.checks.as_ref(), p.head) {
        return Ok(no(reason));
    }
    // Required checks that couldn't be read don't count as none.
    if let Some(reason) = unreadable_required(
        snapshot.required.complete,
        pull.mergeable_state.as_deref(),
        &reading,
    ) {
        return Ok(no(reason));
    }
    // The gate again with the task's current facts (a level raised since the hold now needs its
    // review).
    if let Err(r) = check_mergeable(&gate, &reading.verdict, local_named) {
        return Ok(no(r.reason));
    }
    // A review pass counts only for the exact head it reviewed.
    if let Some(reason) = stale {
        return Ok(no(reason));
    }
    Ok(Verified::Go)
}

/// The policy's own sentence for a moved head ("the branch moved from a1b2c3d to b2c3d4e since
/// the approval; it needs a new approval").
pub(crate) fn moved(approved: &CommitSha, now: &CommitSha) -> String {
    check_approval(
        approved,
        now,
        &qm_policy::merge_gate::ChecksVerdict::Passed,
        false,
    )
    .err()
    .map_or_else(
        || "the branch moved since the approval".to_owned(),
        |r| r.reason,
    )
}

enum Concluded {
    Merged(CommitSha),
    No {
        reason: String,
        superseded: bool,
    },
    /// GitHub answered late or unclearly: neither merged nor not merged yet.
    Unknown(String),
}

/// The words for a merge whose outcome isn't known yet.
fn still_reading(what: &str) -> String {
    format!(
        "{what}; the merge's outcome isn't known yet, so the app keeps reading the pull request on GitHub and records the merge once GitHub confirms it"
    )
}

/// What a merge answer comes to. `may_retry`: an ambiguous answer may be followed by exactly one
/// retry of the same merge with the same SHA.
async fn conclude(
    svc: &SettlingService,
    github: &GithubClient,
    p: &PrMerge<'_>,
    result: MergeResult,
    may_retry: bool,
) -> Concluded {
    match result {
        // Merged only when the read-back says so too.
        MergeResult::Merged { merge_commit } => match read_back(svc, github, p).await {
            ReadBack::Merged(commit) => Concluded::Merged(commit.unwrap_or(merge_commit)),
            ReadBack::Open => Concluded::Unknown(still_reading(&format!(
                "GitHub said it merged PR #{} but still shows it open at {} after 2 minutes",
                p.pr,
                p.head.short()
            ))),
            ReadBack::Unknown => Concluded::Unknown(still_reading(&format!(
                "GitHub said it merged PR #{} but couldn't be read back to confirm it",
                p.pr
            ))),
            ReadBack::Other(reason) => Concluded::No {
                reason,
                superseded: false,
            },
        },
        // The head moved after the re-check; GitHub refused the precondition.
        MergeResult::HeadMoved { message } => Concluded::No {
            reason: format!(
                "GitHub refused the merge because the branch moved from {} ({message}); it needs a new approval",
                p.head.short()
            ),
            superseded: true,
        },
        MergeResult::NotMergeable { message } => Concluded::No {
            reason: format!("GitHub won't merge PR #{}: {message}", p.pr),
            superseded: false,
        },
        // An ambiguous answer is followed by a read-back, never a blind retry.
        MergeResult::Unknown { message } => match read_once(github, p).await {
            ReadBack::Merged(Some(commit)) => Concluded::Merged(commit),
            ReadBack::Open if may_retry => {
                svc.tasks().clock_arc().sleep(RETRY_AFTER).await;
                // The same merge, the same SHA, once.
                let again = github.merge_pr(&p.repo, p.pr, p.head, p.method).await;
                Box::pin(conclude(svc, github, p, again, false)).await
            }
            ReadBack::Merged(None) => Concluded::Unknown(still_reading(&format!(
                "GitHub didn't answer the merge clearly ({message}) and shows PR #{} merged without its merge commit yet",
                p.pr
            ))),
            ReadBack::Open => Concluded::Unknown(still_reading(&format!(
                "GitHub didn't answer the merge clearly ({message}) and still shows PR #{} open at {}",
                p.pr,
                p.head.short()
            ))),
            ReadBack::Unknown => Concluded::Unknown(still_reading(&format!(
                "GitHub didn't answer the merge clearly ({message}) and couldn't be read back"
            ))),
            ReadBack::Other(reason) => Concluded::No {
                reason,
                superseded: false,
            },
        },
    }
}

enum ReadBack {
    /// Merged at exactly the approved head.
    Merged(Option<CommitSha>),
    /// Still open at exactly the approved head.
    Open,
    /// GitHub couldn't be read.
    Unknown,
    /// Anything else, as a reason.
    Other(String),
}

fn classify(pull: &PullRequest, p: &PrMerge<'_>) -> ReadBack {
    if &pull.head_sha != p.head {
        return ReadBack::Other(format!(
            "PR #{} is at {} now, not the approved {}",
            p.pr,
            pull.head_sha.short(),
            p.head.short()
        ));
    }
    if pull.merged {
        return ReadBack::Merged(pull.merge_commit.clone());
    }
    if pull.state == PrState::Open {
        ReadBack::Open
    } else {
        ReadBack::Other(format!("PR #{} was closed without merging", p.pr))
    }
}

async fn read_once(github: &GithubClient, p: &PrMerge<'_>) -> ReadBack {
    match github.pull(&p.repo, p.pr).await {
        Ok(pull) => classify(&pull, p),
        Err(_) => ReadBack::Unknown,
    }
}

/// Reads the PR back until it's merged at the approved head, for at most 2 minutes.
async fn read_back(svc: &SettlingService, github: &GithubClient, p: &PrMerge<'_>) -> ReadBack {
    let clock = svc.tasks().clock_arc();
    let mut last = ReadBack::Unknown;
    for wait in READ_BACK {
        clock.sleep(wait).await;
        last = read_once(github, p).await;
        match last {
            ReadBack::Merged(_) | ReadBack::Other(_) => return last,
            ReadBack::Open | ReadBack::Unknown => {}
        }
    }
    last
}

/// The merge is a fact (the read-back confirmed it): recorded, then the remote task branch deleted
/// (a failure is an event only).
async fn land(
    svc: &SettlingService,
    p: &PrMerge<'_>,
    merge_commit: CommitSha,
) -> Result<Landing, SettleError> {
    let fact = Fact::MergedPr {
        pr: p.pr,
        head: p.head.clone(),
        merge_commit,
        base: p.base.to_owned(),
    };
    let at = svc.delivery().clock().now();
    let parent = svc.delivery().parent();
    let task = p.task;
    let recorded = fact.clone();
    svc.tasks()
        .write(move |tx, notify| facts::landed(tx, notify, &*parent, task, &recorded, at))
        .await?;
    delete_remote_branch(svc, p).await;
    Ok(Landing::Landed(fact))
}

/// After a merge the remote task branch goes, unless GitHub already removed it.
async fn delete_remote_branch(svc: &SettlingService, p: &PrMerge<'_>) {
    if !DELETE_REMOTE_BRANCH_AFTER_MERGE {
        return;
    }
    let branch = p.task.branch();
    if let Err(e) = svc
        .delivery()
        .github()
        .delete_branch(&p.repo, &branch)
        .await
    {
        let at = svc.delivery().clock().now();
        let task = p.task;
        let summary = format!(
            "{task}: the remote branch {branch} couldn't be deleted: {}",
            e.plain()
        );
        let _ = svc
            .tasks()
            .write(move |tx, _| {
                task_store::event(
                    tx,
                    "settle.branch_kept",
                    task,
                    None,
                    &summary,
                    serde_json::json!({}),
                    at,
                )
                .map(drop)
            })
            .await;
    }
}

/// Delete the remote task branch after its pull request merged.
pub const DELETE_REMOTE_BRANCH_AFTER_MERGE: bool = true;

async fn refused(
    svc: &SettlingService,
    task: TaskId,
    head: &CommitSha,
    reason: String,
    superseded: bool,
    origin: Origin,
) -> Result<Landing, SettleError> {
    // Any doubt ends in "not merged", with the reason; the task goes back to reported.
    svc.refuse(task, head, &reason, origin, None).await?;
    Ok(Landing::Refused { reason, superseded })
}
