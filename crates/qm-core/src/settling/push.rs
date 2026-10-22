//! Branch-only delivery: push the branch with the app's lease, never merge it. The task stays
//! `settling` until the checks GitHub runs on the pushed branch allow it to land.

use super::facts::{self, Fact};
use super::land_pr::moved;
use super::{Landing, Origin, SETTLE_PUSHED_EVENT, SettleError, SettlingService, refuse_in};
use crate::delivery::checks::local_checks_named;
use crate::delivery::ports::ParentPort;
use crate::delivery::push_guard::{self, AppPush, Pushed};
use crate::delivery::report_complete;
use crate::inbox::Notify;
use crate::tasks::lifecycle::{TaskEvent, next};
use jiff::Timestamp;
use qm_git::Repo;
use qm_policy::merge_gate::ChecksVerdict;
use qm_store::StoreError;
use qm_store::delivery::{self as delivery_store, ChecksState, verdict};
use qm_store::held::PushPayload;
use qm_store::tasks as task_store;
use qm_store::time::to_millis;
use qm_types::domain::{DeliveryMode, TaskStatus};
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;

/// Pushes `payload` exactly: the local task branch must still be at the approved commit, the repair
/// rule must allow it, and the remote branch must be where the app left it.
#[allow(clippy::too_many_lines)]
pub async fn push_branch(
    svc: &SettlingService,
    payload: &PushPayload,
    origin: Origin,
) -> Result<Landing, SettleError> {
    let task = payload.task;
    let head = &payload.head;
    let _lock = svc.delivery().locks().lock(task).await;
    let loaded = svc
        .delivery()
        .load(task)
        .await
        .map_err(|e| SettleError::Failed(e.render()))?;
    if loaded.row.status != TaskStatus::Settling {
        let reason = format!("{task} is {}, not settling", loaded.row.status);
        return refused(svc, task, head, reason, false, origin).await;
    }
    // The plan for branch-only is a push, never a merge; the local tip must be the approved commit.
    let git = svc.delivery().git();
    let checkout = loaded.project.path.clone();
    let branch = payload.branch.clone();
    let tip = tokio::task::spawn_blocking(move || {
        Repo::open(git, &checkout)?.local_branch_commit(&branch)
    })
    .await
    .map_err(|e| SettleError::Failed(e.to_string()))?
    .map_err(|e| SettleError::Failed(e.to_string()))?;
    if tip.as_ref() != Some(head) {
        let reason = match &tip {
            Some(now) => moved(head, now),
            None => format!("{} doesn't exist any more", payload.branch),
        };
        return refused(svc, task, head, reason, true, origin).await;
    }
    // The repair rule on a re-settle after red checks.
    let pushed = loaded.delivery.pushed_head.clone();
    let failing_now: Vec<String> = loaded
        .delivery
        .checks
        .as_ref()
        .filter(|c| c.is_final && Some(&c.head) == pushed.as_ref() && c.verdict == verdict::FAILED)
        .map(|c| c.failing.clone())
        .unwrap_or_default();
    let port = svc.delivery().verification();
    let checked = failing_now.clone();
    let check_port = std::sync::Arc::clone(&port);
    let (allowed, complete) = svc
        .tasks()
        .db()
        .call(move |conn| {
            let complete =
                qm_store::reports::latest_clear(conn, task)?.is_some_and(|r| report_complete(&r));
            Ok((
                check_port.before_push(conn, task, &checked, complete)?,
                complete,
            ))
        })
        .await?;
    if let Err(r) = allowed {
        return refused(svc, task, head, r.reason, false, origin).await;
    }

    // The push, in the app's environment with its own credentials, leased, only to the project's
    // repository, and confirmed on GitHub before it's recorded: a lease rejected where GitHub
    // already has this head counts as pushed.
    let outcome = push_guard::push(
        svc.delivery(),
        &AppPush {
            project: &loaded.project,
            remote: &payload.remote,
            branch: &payload.branch,
            head,
            expected_remote: payload.expected_remote.as_ref(),
            retry: "settle again",
        },
    )
    .await;
    let reason = match outcome {
        Pushed::Confirmed => None,
        Pushed::Refused(reason) | Pushed::Failed(reason) => Some(reason),
        Pushed::Rejected(message) => {
            Some(format!("the remote refused {}: {message}", payload.branch))
        }
    };
    if let Some(reason) = reason {
        return refused(svc, task, head, reason, false, origin).await;
    }

    // One transaction: the push recorded, its checks pending. Without a GitHub repository no
    // checks can be read, so the local-checks rule decides at once.
    let at = svc.delivery().clock().now();
    let has_github = loaded.repo().is_some();
    let parent = svc.delivery().parent();
    let record_head = head.clone();
    let branch = payload.branch.clone();
    svc.tasks()
        .write(move |tx, notify| {
            let state = ChecksState::pushed(record_head.clone(), &branch, to_millis(at));
            delivery_store::record_push(tx, task, None, &record_head, &state)?;
            port.pushed(tx, task, &record_head, &failing_now, complete)?;
            task_store::event(
                tx,
                SETTLE_PUSHED_EVENT,
                task,
                None,
                &format!("{task} pushed {branch} at {}", record_head.short()),
                serde_json::json!({ "branch": branch, "head": record_head }),
                at,
            )?;
            if !has_github {
                let mut done = state;
                done.is_final = true;
                verdict::NO_CHECKS.clone_into(&mut done.verdict);
                delivery_store::set_checks(tx, task, &done)?;
                branch_checks_final(
                    tx,
                    notify,
                    &*parent,
                    task,
                    &record_head,
                    &ChecksVerdict::NoChecks,
                    at,
                )?;
            }
            Ok(())
        })
        .await?;
    if has_github {
        svc.delivery().arm(task, head.clone());
    }
    Ok(Landing::Pushed {
        branch: payload.branch.clone(),
        head: head.clone(),
    })
}

/// A branch-only task in `settling` got its pushed head's final verdict, in the verdict's
/// transaction: passed, or no checks with Verified naming local checks on that head, lands it as
/// delivered; anything else sends it back to its crewmate (`settling → reported → running`) and
/// wakes the supervisor with `settle_failed`. The crewmate's `checks` wake is posted by the poller
/// in the same transaction.
pub(crate) fn branch_checks_final(
    tx: &Connection,
    notify: &mut Notify,
    parent: &dyn ParentPort,
    task: TaskId,
    head: &CommitSha,
    verdict: &ChecksVerdict,
    at: Timestamp,
) -> Result<(), StoreError> {
    let Some(row) = task_store::load(tx, task)? else {
        return Ok(());
    };
    if row.status != TaskStatus::Settling || row.delivery != DeliveryMode::BranchOnly {
        return Ok(());
    }
    let named = local_checks_named(tx, task, head)?;
    let delivered = match verdict {
        ChecksVerdict::Passed => true,
        ChecksVerdict::NoChecks => named,
        _ => false,
    };
    if delivered {
        // Delivered, never merged.
        facts::landed(
            tx,
            notify,
            parent,
            task,
            &Fact::Delivered {
                branch: row.branch.clone(),
                head: head.clone(),
            },
            at,
        )?;
        return Ok(());
    }
    let (reason, text) = match verdict {
        ChecksVerdict::Failed(names) | ChecksVerdict::MissingRequired(names) => (
            format!(
                "its pushed branch failed checks on {} ({})",
                head.short(),
                names.join(", ")
            ),
            format!(
                "{task}'s pushed branch failed checks on {} ({}). It isn't delivered; {task} is fixing it. Settle again when it reports done.",
                head.short(),
                names.join(", ")
            ),
        ),
        _ => (
            format!(
                "no checks ran on {} and Verified names no local checks",
                head.short()
            ),
            format!(
                "No checks ran on {task}'s pushed branch at {} and its report's Verified names no local checks, so it isn't delivered; {task} is back to running. Settle again when it reports done.",
                head.short()
            ),
        ),
    };
    refuse_in(
        tx,
        notify,
        task,
        head,
        &reason,
        Origin::Auto,
        Some(text),
        at,
    )?;
    if let Some(now) = task_store::load(tx, task)?
        && let Ok(to) = next(now.status, TaskEvent::Steered)
    {
        task_store::set_status(tx, task, now.status, to, at)?;
    }
    Ok(())
}

async fn refused(
    svc: &SettlingService,
    task: TaskId,
    head: &CommitSha,
    reason: String,
    superseded: bool,
    origin: Origin,
) -> Result<Landing, SettleError> {
    svc.refuse(task, head, &reason, origin, None).await?;
    Ok(Landing::Refused { reason, superseded })
}
