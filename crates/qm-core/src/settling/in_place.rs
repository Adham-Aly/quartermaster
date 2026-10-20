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

/// The merge commit's message when build in place can't fast-forward.
pub fn in_place_message(branch: &str, title: &str) -> String {
    format!("Merge {branch}: {title}")
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
    let models = svc
        .delivery()
        .read(|conn| models(conn))
        .await
        .map_err(|e| SettleError::Failed(e.render()))?;
    let git = svc.delivery().git();
    let checkout_path = PathBuf::from(checkout);
    let (names_git, names_path, names_head, names_target) = (
        git.clone(),
        checkout_path.clone(),
        head.clone(),
        loaded.target(),
    );
    let own = tokio::task::spawn_blocking(move || {
        Repo::open(names_git, &names_path)?.own_commits(&names_head, &names_target)
    })
    .await
    .map_err(|e| SettleError::Failed(e.to_string()))?;
    let names = match own {
        Ok(own) => check_commits(
            own.commits.iter().map(|(_, m)| m.as_str()),
            &models,
            Boundary::LandingInPlace,
        )
        .map_err(|r| r.reason),
        Err(e) => Err(format!(
            "the task's commit names couldn't be read from git ({e}), so they can't be checked"
        )),
    };
    if let Err(reason) = names {
        return refused(svc, task, head, reason, false, origin).await;
    }
    let message = in_place_message(branch, &loaded.row.title);
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
