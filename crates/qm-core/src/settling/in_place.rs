//! Build in place and the personal workspace.

use super::facts::{self, Fact};
use super::land_pr::moved;
use super::{Landing, Origin, SettleError, SettlingService};
use crate::delivery::models;
use qm_git::{InPlaceMerge, InPlaceOutcome, Repo};
use qm_policy::delivery::check_checkout_clean;
use qm_policy::names::{Boundary, check_commits};
use qm_store::held::MergePayload;
use qm_types::domain::TaskStatus;
use qm_types::ids::{CommitSha, TaskId};
use std::path::PathBuf;

/// The merge commit's message when build in place can't fast-forward: git's own wording, so the
/// history reads like any other merge.
pub fn in_place_message(branch: &str) -> String {
    format!("Merge branch '{branch}'")
}

/// Merges `payload` (a `MergePayload::InPlace`) into your checkout, exactly as stored.
pub async fn merge_in_place(
    svc: &SettlingService,
    payload: &MergePayload,
    origin: Origin,
) -> Result<Landing, SettleError> {
    let MergePayload::InPlace {
        task,
        branch,
        head,
        checkout,
        target_branch,
    } = payload
    else {
        return Err(SettleError::Failed(
            "merge_in_place was given a merge that isn't build in place".to_owned(),
        ));
    };
    let task = *task;
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
    // The task's own commit names again, on exactly the approved head.
    if let Err(reason) = check_names(svc, PathBuf::from(checkout), head, loaded.target()).await? {
        return refused(svc, task, head, reason, false, origin).await;
    }
    let git = svc.delivery().git();
    let checkout_path = PathBuf::from(checkout);
    let message = in_place_message(branch);
    let (b, h, t) = (branch.clone(), head.clone(), target_branch.clone());
    // Under the repository lock, exactly the approved commit into exactly the approved branch of a
    // clean checkout; a conflict leaves everything as it was.
    let outcome = tokio::task::spawn_blocking(move || {
        Repo::open(git, &checkout_path)?.merge_in_place(&InPlaceMerge {
            target_branch: &t,
            branch: &b,
            head: &h,
            message: &message,
        })
    })
    .await
    .map_err(|e| SettleError::Failed(e.to_string()))?;
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            let reason = format!("the merge into your checkout failed and was undone: {e}");
            return refused(svc, task, head, reason, false, origin).await;
        }
    };
    match outcome {
        InPlaceOutcome::Merged { new_head, .. } => {
            let fact = Fact::InPlace {
                branch: branch.clone(),
                head: head.clone(),
                new_head,
                target: target_branch.clone(),
            };
            let at = svc.delivery().clock().now();
            let parent = svc.delivery().parent();
            let recorded = fact.clone();
            svc.tasks()
                .write(move |tx, notify| facts::landed(tx, notify, &*parent, task, &recorded, at))
                .await?;
            Ok(Landing::Landed(fact))
        }
        InPlaceOutcome::NotClean => {
            let reason = check_checkout_clean(Some(false))
                .err()
                .map_or_else(String::new, |r| r.reason);
            refused(svc, task, head, reason, false, origin).await
        }
        InPlaceOutcome::WrongBranch { current } => {
            let reason = match current {
                Some(current) => format!("your checkout is on {current} now, not {target_branch}"),
                None => format!("your checkout's HEAD is detached, not on {target_branch}"),
            };
            refused(svc, task, head, reason, true, origin).await
        }
        InPlaceOutcome::HeadMoved { now } => {
            let reason = match now {
                Some(now) => moved(head, &now),
                None => format!("{branch} doesn't exist any more"),
            };
            refused(svc, task, head, reason, true, origin).await
        }
        InPlaceOutcome::Conflict { files } => {
            let reason = format!(
                "merging {branch} into {target_branch} conflicts in {}, and nothing in your checkout was changed",
                files.join(", ")
            );
            refused(svc, task, head, reason, false, origin).await
        }
    }
}

/// The task's own commit names up to `head` (against `target`), checked as at settling; the
/// reason when they fail or can't be read.
async fn check_names(
    svc: &SettlingService,
    checkout: PathBuf,
    head: &CommitSha,
    target: String,
) -> Result<Result<(), String>, SettleError> {
    let models = svc
        .delivery()
        .read(|conn| models(conn))
        .await
        .map_err(|e| SettleError::Failed(e.render()))?;
    let git = svc.delivery().git();
    let head = head.clone();
    let own = tokio::task::spawn_blocking(move || {
        Repo::open(git, &checkout)?.own_commits(&head, &target)
    })
    .await
    .map_err(|e| SettleError::Failed(e.to_string()))?;
    Ok(match own {
        Ok(own) => check_commits(
            own.commits.iter().map(|(_, m)| m.as_str()),
            &models,
            Boundary::LandingInPlace,
        )
        .map_err(|r| r.reason),
        Err(e) => Err(format!(
            "the task's commit names couldn't be read from git ({e}), so they can't be checked"
        )),
    })
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
