//! `pr_open` and `pr_status`.

use super::outside;
use super::push_guard::{self, AppPush, Pushed};
use super::{DeliveryService, callers_task, missing_body_headings, models};
use crate::held::HeldService;
use crate::inbox::Inbox;
use crate::tasks::lifecycle::{TaskEvent, next};
use crate::tasks::{failed, new_wake, own_commits_failure, refuse};
use qm_git::{ORIGIN, Repo};
use qm_github::PrState as GhState;
use qm_policy::merge_gate::check_pr_open;
use qm_policy::names::{Boundary, check_commits};
use qm_policy::verification::effective;
use qm_store::delivery::{self as delivery_store, ChecksState, verdict};
use qm_store::tasks as task_store;
use qm_store::time::to_millis;
use qm_tools::defs::{PrOpened, PrState, PrStatus};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{TaskStatus, VerificationLevel, WakeKind};

/// `events.kind` of a first pull request.
pub const PR_OPENED_EVENT: &str = "pr.opened";
/// `events.kind` of a pull request updated by a push or a new title and body.
pub const PR_UPDATED_EVENT: &str = "pr.updated";
/// `events.kind` of a pending approval made void by a new push.
pub const SUPERSEDED_EVENT: &str = "settle.superseded";

/// `pr_open`: the gate, the body, the names and the repair rule, then the push with the app's own
/// lease and credentials, the pull request, and one transaction recording it all.
#[allow(clippy::too_many_lines)]
pub(crate) async fn open(
    svc: &DeliveryService,
    caller: &Caller,
    title: String,
    body: String,
) -> Result<PrOpened, ToolFailure> {
    let task = callers_task(caller)?;
    let _lock = svc.locks().lock(task).await;
    // A merge GitHub hasn't confirmed yet could still land at the pushed head: nothing moves it
    // until GitHub settles it (the lock is free between its read-backs).
    let waiting = svc
        .read(move |conn| crate::settling::recover::merge_unconfirmed(conn, task))
        .await?;
    if let Some(head) = waiting {
        return Err(refuse(crate::settling::recover::merge_waiting(&head)));
    }
    let loaded = svc.load(task).await?;
    let row = &loaded.row;
    let Some(repo) = loaded.repo() else {
        return Err(refuse(format!(
            "pr_open needs a GitHub remote; {} has none",
            loaded.project.name
        )));
    };
    let target = loaded.target();

    // The worktree's head and the task's own commit names, off the async threads.
    let git = svc.git();
    let checkout = loaded.project.path.clone();
    let worktree = svc.tasks().deps().data_dir.worktree(task);
    let branch = target.clone();
    let (head, own) = tokio::task::spawn_blocking(move || {
        let repo = Repo::open(git, &checkout)?;
        let Some(head) = repo.worktree_head(&worktree)? else {
            return Ok(None);
        };
        let own = repo.own_commits(&head, &branch)?;
        Ok::<_, qm_git::GitError>(Some((head, own)))
    })
    .await
    .map_err(|e| failed(&e))?
    .map_err(|e| own_commits_failure(&e))?
    .ok_or_else(|| refuse("your branch has no commit yet; commit your work first"))?;
    let messages = own.commits;
    if messages.is_empty() {
        return Err(refuse(format!(
            "your branch has no commits of its own on top of {}; commit your work before pr_open",
            own.against
        )));
    }

    // The body: a Summary and a Testing section at standard and reviewed, optional at quick.
    let level = row.verification.map(|(l, _)| effective(l, row.delivery).0);
    let missing = missing_body_headings(&body);
    let body_complete = missing.is_empty();
    if matches!(
        level,
        Some(VerificationLevel::Standard | VerificationLevel::Reviewed)
    ) && !body_complete
    {
        return Err(refuse(format!(
            "The pull request body needs a Summary and a Testing section; missing: {}",
            missing.join(", ")
        )));
    }

    // The gate, with a raised level's validation counted as done by a body that says how it was
    // tested.
    let verification = svc.verification();
    let gate_port = std::sync::Arc::clone(&verification);
    let (mut gate, models) = svc
        .read(move |conn| Ok((gate_port.gate(conn, task)?, models(conn)?)))
        .await?;
    if body_complete {
        gate.validation_pending = false;
    }
    check_pr_open(&gate)?;
    check_commits(
        messages.iter().map(|(_, m)| m.as_str()),
        &models,
        Boundary::PrOpen,
    )?;

    let pushed = loaded.delivery.pushed_head.clone();
    // Nothing new to push: the title and body only.
    if pushed.as_ref() == Some(&head)
        && let Some(pr) = loaded.delivery.pr_number
    {
        let updated = svc
            .github()
            .update_pr(&repo, pr, &title, &body)
            .await
            .map_err(|e| ToolFailure::Failed(e.plain()))?;
        let at = svc.clock().now();
        let summary = format!("PR #{pr} updated (title and description)");
        svc.tasks()
            .write(move |tx, _| {
                task_store::event(
                    tx,
                    PR_UPDATED_EVENT,
                    task,
                    None,
                    &summary,
                    serde_json::json!({ "pr": pr }),
                    at,
                )
                .map(drop)
            })
            .await
            .map_err(|e| failed(&e))?;
        return Ok(PrOpened {
            pr,
            url: updated.url,
            head,
        });
    }

    // A fix for a failing check needs a declared diagnosis (and a raised level its validation),
    // checked only when something will be pushed.
    let failing_now: Vec<String> = loaded
        .delivery
        .checks
        .as_ref()
        .filter(|c| c.is_final && Some(&c.head) == pushed.as_ref() && c.verdict == verdict::FAILED)
        .map(|c| c.failing.clone())
        .unwrap_or_default();
    let push_port = std::sync::Arc::clone(&verification);
    let checked = failing_now.clone();
    svc.read(move |conn| push_port.before_push(conn, task, &checked, body_complete))
        .await??;

    // The push: the app's own, in its own environment, leased on its own last push, only to the
    // project's repository, and confirmed on GitHub before anything is recorded (an earlier push of
    // exactly this commit that the app didn't get to record counts too).
    let outcome = push_guard::push(
        svc,
        &AppPush {
            project: &loaded.project,
            remote: ORIGIN,
            branch: &row.branch,
            head: &head,
            expected_remote: pushed.as_ref(),
            retry: "call pr_open again",
        },
    )
    .await;
    match outcome {
        Pushed::Confirmed => {}
        Pushed::Refused(reason) => return Err(refuse(reason)),
        Pushed::Rejected(message) => {
            return Err(ToolFailure::Failed(format!(
                "GitHub refused the push of {}: {message}",
                row.branch
            )));
        }
        Pushed::Failed(message) => return Err(ToolFailure::Failed(message)),
    }

    // The pull request.
    let existing = svc
        .github()
        .find_open_pr(&repo, &row.branch)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    let (pull, first) = match existing {
        Some(pull) => (
            svc.github()
                .update_pr(&repo, pull.number, &title, &body)
                .await
                .map_err(|e| ToolFailure::Failed(e.plain()))?,
            false,
        ),
        None => (
            svc.github()
                .create_pr(&repo, &title, &body, &row.branch, &target)
                .await
                .map_err(|e| ToolFailure::Failed(e.plain()))?,
            true,
        ),
    };
    if pull.state != GhState::Open {
        return Err(ToolFailure::Failed(format!(
            "PR #{} isn't open on GitHub",
            pull.number
        )));
    }

    // One transaction: the push, superseding stale approvals, verification's note, the event.
    let at = svc.clock().now();
    let pr = pull.number;
    let supervisor = row.supervisor;
    let record_head = head.clone();
    let record_port = verification;
    let required_branch = target.clone();
    svc.tasks()
        .write(move |tx, notify| {
            let short = record_head.short().to_owned();
            // A new push supersedes every pending approval of this task, before the new pushed head
            // is recorded.
            let why = format!("the branch moved to {short}");
            let mut superseded = false;
            for held in delivery_store::pending_settle_holds(tx, task)? {
                if HeldService::supersede_with(tx, held, Some(task), &why, at)? {
                    superseded = true;
                    task_store::event(
                        tx,
                        SUPERSEDED_EVENT,
                        task,
                        None,
                        &format!("{task}'s approval no longer applies: {why}"),
                        serde_json::json!({ "held": held, "head": record_head }),
                        at,
                    )?;
                }
            }
            if let Some(now) = task_store::load(tx, task)?
                && now.status == TaskStatus::Settling
                && let Ok(to) = next(now.status, TaskEvent::SettleRefused)
            {
                task_store::set_status(tx, task, now.status, to, at)?;
                if superseded {
                    let text = format!(
                        "{task}'s branch moved to {short} while it waited for the user's approval, so nothing was merged and it's back to reported. Settle again once its checks pass on the new head."
                    );
                    let event = task_store::event(
                        tx,
                        "settle.refused",
                        task,
                        None,
                        &format!("{task} not settled: {why}"),
                        serde_json::json!({ "reason": why }),
                        at,
                    )?;
                    Inbox::post(
                        tx,
                        &new_wake(
                            supervisor,
                            WakeKind::SettleFailed,
                            task,
                            format!("settle_failed:{task}:{short}:{event}"),
                            &text,
                            &format!("{task} not settled"),
                            serde_json::json!({}),
                            at,
                        ),
                        notify,
                    )?;
                }
            }
            let state = ChecksState::pushed(record_head.clone(), &required_branch, to_millis(at));
            delivery_store::record_push(tx, task, Some(pr), &record_head, &state)?;
            record_port.pushed(tx, task, &record_head, &failing_now, body_complete)?;
            let (kind, summary) = if first {
                (
                    PR_OPENED_EVENT,
                    format!("{task} opened PR #{pr} at {short}"),
                )
            } else {
                (PR_UPDATED_EVENT, format!("PR #{pr} updated to {short}"))
            };
            task_store::event(
                tx,
                kind,
                task,
                None,
                &summary,
                serde_json::json!({ "pr": pr, "head": record_head }),
                at,
            )?;
            Ok(())
        })
        .await
        .map_err(|e| failed(&e))?;

    svc.arm(task, head.clone());
    outside::arm(svc, task, outside::WATCH_EVERY);
    Ok(PrOpened {
        pr,
        url: pull.url,
        head,
    })
}

/// `pr_status`: the caller's pull request as GitHub reports it now.
pub(crate) async fn status(
    svc: &DeliveryService,
    caller: &Caller,
) -> Result<PrStatus, ToolFailure> {
    let task = callers_task(caller)?;
    let loaded = svc.load(task).await?;
    let (Some(pr), Some(repo)) = (loaded.delivery.pr_number, loaded.repo()) else {
        return Err(refuse(
            "You haven't opened a pull request yet; pr_open opens it",
        ));
    };
    let pull = svc
        .github()
        .pull(&repo, pr)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    Ok(PrStatus {
        pr,
        url: pull.url,
        state: match (pull.merged, pull.state) {
            (true, _) => PrState::Merged,
            (false, GhState::Open) => PrState::Open,
            (false, GhState::Closed) => PrState::Closed,
        },
        head: pull.head_sha,
        mergeable: pull.mergeable,
    })
}
