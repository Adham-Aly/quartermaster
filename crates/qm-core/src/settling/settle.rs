//! `settle_task`.
//!
//! Who and what; answer-only work closes; code work needs a clear done report, landed blockers,
//! good names, the agreed head and the merge gate. Then `qm_policy::delivery::plan` decides: with
//! autonomy normal one held action is written, once, with its exact payload; under auto the work
//! is landed now by the same functions the performer uses.

use super::in_place::merge_in_place;
use super::land_pr::{land_pr, unreadable_required, vanished_checks};
use super::push::push_branch;
use super::review::{passed_for, review_refusal};
use super::{Fact, Landing, Origin, SETTLE_HELD_EVENT, SettlingService, facts};
use crate::delivery::checks::read_snapshot;
use crate::delivery::outside::{self, PrSeen};
use crate::delivery::{models, verified_named};
use crate::held::HeldService;
use crate::tasks::lifecycle::{TaskEvent, next_for};
use crate::tasks::waits::check_direct;
use crate::tasks::{failed, refuse};
use qm_git::{ORIGIN, Repo};
use qm_github::PrState;
use qm_policy::delivery::{Deliverable, SettleFacts, SettlePlan, plan};
use qm_policy::merge_gate::{ChecksVerdict, check_mergeable};
use qm_policy::names::{Boundary, check_commits, check_pr_title};
use qm_store::StoreError;
use qm_store::delivery as delivery_store;
use qm_store::held::{HeldPayload, MergePayload, NewHeld, PushPayload};
use qm_store::reports;
use qm_store::reports::ReportRow;
use qm_store::tasks as task_store;
use qm_store::tasks::TaskRow;
use qm_tools::defs::SettleOutcome;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{Autonomy, DeliveryMode, ReportOutcome, TaskOutcome, TaskStatus};
use qm_types::ids::{CommitSha, HeldActionId, ReportId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// Settles folder work: marks the task delivered at once, in one transaction, with the folder it
/// is in. Nothing is held, merged, pushed or named: there is no branch or commit to check.
async fn settle_folder(
    svc: &SettlingService,
    task: TaskId,
    row: &TaskRow,
    report: &ReportRow,
    folder: String,
    autonomy: Autonomy,
) -> Result<SettleOutcome, ToolFailure> {
    let port = svc.delivery().verification();
    let gate = svc
        .delivery()
        .read(move |conn| port.gate(conn, task))
        .await?;
    check_mergeable(&gate, &ChecksVerdict::NoChecks, verified_named(report))?;
    match plan(&SettleFacts {
        deliverable: Deliverable::Code,
        delivery: row.delivery,
        autonomy,
        has_remote: false,
        head: None,
        checkout_clean: None,
    })? {
        SettlePlan::MarkDone => {}
        other => {
            return Err(ToolFailure::Failed(format!(
                "folder work should settle as done, not {other:?}"
            )));
        }
    }
    let at = svc.delivery().clock().now();
    let parent = svc.delivery().parent();
    let fact = Fact::DoneInFolder {
        folder: folder.clone(),
    };
    let tasks = svc.tasks();
    let refused = tasks
        .write(move |tx, notify| {
            let Some(now) = task_store::load(tx, task)? else {
                return Ok(Some(format!("{task} doesn't exist any more")));
            };
            if now.status != TaskStatus::Reported {
                return Ok(Some(format!(
                    "{task} is {} now, not reported, so it can't be settled",
                    now.status
                )));
            }
            let to = match next_for(task, now.status, TaskEvent::SettleStarted) {
                Ok(to) => to,
                Err(r) => return Ok(Some(r.reason)),
            };
            task_store::set_status(tx, task, now.status, to, at)?;
            if !facts::landed(tx, notify, &*parent, task, &fact, at)? {
                return Err(StoreError::Invalid {
                    column: "tasks.status",
                    message: format!("{task} couldn't be recorded as delivered"),
                });
            }
            Ok(None)
        })
        .await
        .map_err(|e| failed(&e))?;
    if let Some(reason) = refused {
        return Err(refuse(reason));
    }
    Ok(SettleOutcome::Done { folder })
}

/// `settle_task`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn settle(
    svc: &SettlingService,
    caller: &Caller,
    task: TaskId,
) -> Result<SettleOutcome, ToolFailure> {
    let tasks = svc.tasks();
    // 1. Who and what.
    let loaded = svc.delivery().load(task).await?;
    check_direct(tasks, caller, &loaded.row).await?;
    let report = svc
        .delivery()
        .read(move |conn| reports::latest_clear(conn, task))
        .await?;
    let done = report
        .as_ref()
        .is_some_and(|r| r.outcome == Some(ReportOutcome::Done));

    // 2. An answer, whatever the kind: nothing lands; the answer is kept and the task closes. A
    // task that hasn't reported done yet goes on to the refusal below.
    let delivered = crate::tasks::delivered::of_report(&loaded.row, report.as_ref());
    if crate::tasks::review::awaits_go_ahead(&loaded.row) {
        return Err(refuse(format!(
            "{task}'s review report waits for the user's go-ahead on its card; settle it after \
             they give it (approve_review records a go-ahead they give you in chat)"
        )));
    }
    if done && delivered == Deliverable::Answer {
        if loaded.row.status == TaskStatus::Reported && done {
            tasks.close_task(caller, task).await?;
            return Ok(SettleOutcome::Closed);
        }
        return Err(refuse(format!(
            "{task} hasn't reported its answer yet (it's {})",
            loaded.row.status
        )));
    }

    let lock = svc.delivery().locks().lock(task).await;
    // Merged or tagged on GitHub outside the app: recorded and told, and never merged again.
    let watch = svc.delivery().clone();
    tokio::spawn(async move {
        if let Err(e) = outside::check_tags(&watch, task).await {
            tracing::warn!(%task, error = %e, "couldn't list the tags at settle");
        }
    });
    match outside::check_pr_locked(svc.delivery(), task).await {
        Ok(PrSeen::MergedOutside { pr, merge_commit }) => {
            return Ok(SettleOutcome::Merged {
                pr,
                commit: merge_commit,
            });
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%task, error = %e, "couldn't read the pull request at settle"),
    }
    let loaded = svc.delivery().load(task).await?;
    let row = &loaded.row;

    // 3. The status: reported with a done report; a settle already waiting is answered again.
    if row.status == TaskStatus::Settling {
        let pending = svc
            .delivery()
            .read(move |conn| delivery_store::pending_settle_holds(conn, task))
            .await?;
        if let Some(held) = pending.last() {
            return Ok(SettleOutcome::Held {
                held_action: *held,
                note: format!(
                    "{task} is already waiting for the user's approval; the answer comes back to you."
                ),
            });
        }
        return Err(refuse(format!("{task} is already being settled")));
    }
    if let Some(why) = svc
        .delivery()
        .read(move |conn| {
            crate::jobs::helpers_running(conn, task, crate::jobs::HelperWait::Supervisor)
        })
        .await?
    {
        return Err(refuse(why));
    }
    let Some(report) = report.filter(|_| row.status == TaskStatus::Reported && done) else {
        return Err(refuse(format!(
            "{task} hasn't reported done yet (it's {})",
            row.status
        )));
    };
    // 4. Blockers: every one landed (or closed as landed or answered).
    let blockers = svc
        .delivery()
        .read(move |conn| unfinished_blocker(conn, task))
        .await?;
    if let Some(blocker) = blockers {
        return Err(refuse(format!(
            "{task} waits on {blocker}, which hasn't landed yet; if that dependency is no longer required, call unlink_tasks with blocker={blocker} and blocked={task}, then retry settle_task"
        )));
    }

    // Folder work has no branch, commits or checks: its changes are already in the folder. The
    // verification gate and the report still apply; nothing is held, merged or pushed.
    if row.delivery == DeliveryMode::Folder {
        let folder = loaded.project.path.to_string_lossy().into_owned();
        return settle_folder(svc, task, row, &report, folder, loaded.project.autonomy).await;
    }

    let Some(reported_head) = report
        .head_commit
        .as_deref()
        .and_then(|h| h.parse::<CommitSha>().ok())
    else {
        return Err(refuse(format!(
            "{task}'s done report names no head commit, so there's nothing to settle"
        )));
    };

    // 5. Names at settling on exactly the commits being settled (the task's own up to the reported
    // head, which the agreed-head check below holds every other head to), and the facts of the
    // checkout.
    let git = svc.delivery().git();
    let checkout = loaded.project.path.clone();
    let base = row.base_commit.clone();
    let target = loaded.target();
    let target_of_names = target.clone();
    let branch = row.branch.clone();
    let in_place = row.delivery == DeliveryMode::BuildInPlace;
    let settled_head = reported_head.clone();
    let local = tokio::task::spawn_blocking(move || {
        let repo = Repo::open(git, &checkout)?;
        let messages = repo
            .own_commits(&settled_head, &target_of_names)
            .map(|own| own.commits)
            .map_err(|e| e.to_string());
        let tip = repo.local_branch_commit(&branch)?;
        let has_origin = repo.remotes()?.iter().any(|r| r == ORIGIN);
        let (clean, current) = if in_place {
            (Some(repo.is_clean()?), repo.current_branch()?)
        } else {
            (None, None)
        };
        let stat = match (&base, &tip) {
            (Some(base), Some(tip)) => repo.diff_stat(base, tip).ok(),
            _ => None,
        };
        Ok::<_, qm_git::GitError>(LocalFacts {
            messages: messages.map(|m| m.into_iter().map(|(_, m)| m).collect()),
            tip,
            has_origin,
            clean,
            current,
            stat,
        })
    })
    .await
    .map_err(|e| failed(&e))?
    .map_err(|e| failed(&e))?;
    let messages = local.messages.as_ref().map_err(|e| {
        refuse(format!(
            "the commit names of {task} up to {} couldn't be read from git ({e}), so they can't be checked",
            reported_head.short()
        ))
    })?;
    let models = svc.delivery().read(|conn| models(conn)).await?;
    check_commits(
        messages.iter().map(String::as_str),
        &models,
        Boundary::Settling,
    )?;

    // 6. The agreed head, and 7. the checks verdict on it.
    let repo = loaded.repo();
    let mut card = CardFacts::default();
    let (head, verdict, pr) = match row.delivery {
        DeliveryMode::DirectPr => {
            let (Some(repo), Some(pr)) = (repo.as_ref(), loaded.delivery.pr_number) else {
                return Err(refuse(format!(
                    "{task} has no pull request; its crewmate opens it with pr_open"
                )));
            };
            let pull = svc.delivery().github().pull(repo, pr).await.map_err(|e| {
                ToolFailure::Failed(format!(
                    "GitHub couldn't be read, so PR #{pr}'s state isn't known: {}",
                    e.plain()
                ))
            })?;
            if pull.merged {
                // Already merged at exactly the approved head (a merge GitHub finished late, or
                // one confirmed after the app gave up waiting): the merge is recorded.
                let pushed = loaded.delivery.pushed_head.as_ref();
                if pull.head_sha == reported_head
                    && pushed == Some(&pull.head_sha)
                    && pull.base_ref == target
                    && let Some(merge_commit) = pull.merge_commit.clone()
                {
                    return record_merged(svc, task, pr, pull.head_sha, merge_commit, target).await;
                }
                return Err(refuse(format!(
                    "PR #{pr} is already merged on GitHub, but at {}, not the reported and pushed {}; nothing is recorded",
                    pull.head_sha.short(),
                    reported_head.short()
                )));
            }
            if pull.state != PrState::Open {
                return Err(refuse(format!(
                    "PR #{pr} isn't merged: it's closed on GitHub"
                )));
            }
            if pull.base_ref != target {
                return Err(refuse(format!(
                    "PR #{pr} targets {}, not {target}",
                    pull.base_ref
                )));
            }
            if pull.head_repo.as_ref() != Some(repo) {
                return Err(refuse(format!("PR #{pr}'s branch isn't in {repo}")));
            }
            // The title as it is on GitHub now (the user may have edited it).
            check_pr_title(&pull.title, Boundary::Settling)?;
            // GitHub's head, the app's pushed head and the reported head agree exactly.
            let Some(pushed) = loaded.delivery.pushed_head.as_ref() else {
                return Err(refuse(format!(
                    "{task}'s head was never pushed by the app; its crewmate pushes with pr_open"
                )));
            };
            agreed(task, &pull.head_sha, Some(pushed), &reported_head)?;
            let snapshot = svc
                .delivery()
                .github()
                .checks(repo, &pull.head_sha, &target)
                .await
                .map_err(|e| ToolFailure::Failed(e.plain()))?;
            let reading = read_snapshot(&snapshot);
            // Before any hold: at least the checks the poller saw on this head.
            if let Some(reason) =
                vanished_checks(&reading, loaded.delivery.checks.as_ref(), &pull.head_sha)
            {
                return Err(refuse(reason));
            }
            // Before any hold: unreadable required checks need GitHub's own "clean" and
            // every reported check passing.
            if let Some(reason) = unreadable_required(
                snapshot.required.complete,
                pull.mergeable_state.as_deref(),
                &reading,
            ) {
                return Err(refuse(reason));
            }
            card.additions = pull.additions;
            card.deletions = pull.deletions;
            card.checks = Some((reading.passed, reading.total));
            (pull.head_sha, reading.verdict, Some(pr))
        }
        DeliveryMode::Folder => {
            // Folder work settled above, before any git read.
            return Err(failed(&format!(
                "{task} is folder work and has no git head"
            )));
        }
        DeliveryMode::BranchOnly | DeliveryMode::BuildInPlace => {
            // The local branch tip is the reported head.
            let Some(tip) = local.tip.clone() else {
                return Err(refuse(format!("{} doesn't exist", row.branch)));
            };
            agreed(task, &tip, None, &reported_head)?;
            if let Some(stat) = local.stat {
                card.additions = stat.additions;
                card.deletions = stat.deletions;
            }
            (tip, ChecksVerdict::NoChecks, None)
        }
    };
    let local_named =
        verified_named(&report) && report.head_commit.as_deref() == Some(head.as_str());

    // The gate runs before any hold is written.
    let port = svc.delivery().verification();
    let gate = svc
        .delivery()
        .read(move |conn| port.gate(conn, task))
        .await?;
    check_mergeable(&gate, &verdict, local_named)?;
    // A Careful task's review pass counts only for the exact head it reviewed.
    let gate_head = head.clone();
    let stale = svc
        .delivery()
        .read(move |conn| review_refusal(conn, task, gate, &gate_head))
        .await?;
    if let Some(reason) = stale {
        return Err(refuse(reason));
    }

    // 8. The plan, with autonomy read now.
    let has_remote = match row.delivery {
        DeliveryMode::DirectPr => repo.is_some(),
        _ => local.has_origin,
    };
    let settle_plan = plan(&SettleFacts {
        deliverable: Deliverable::Code,
        delivery: row.delivery,
        autonomy: loaded.project.autonomy,
        has_remote,
        head: Some(&head),
        checkout_clean: local.clean,
    })?;
    let target_branch = if in_place {
        Some(local.current.clone().ok_or_else(|| {
            refuse("your checkout's HEAD is detached, so there's no branch to land the work in")
        })?)
    } else {
        None
    };
    let inner = match &settle_plan {
        SettlePlan::Hold(inner) => inner.as_ref().clone(),
        other => other.clone(),
    };
    // Branch-only plans never merge.
    debug_assert!(row.delivery != DeliveryMode::BranchOnly || !settle_plan.merges());
    let payload = match &inner {
        SettlePlan::MergePr { head, method } => {
            let (Some(repo), Some(pr)) = (repo.as_ref(), pr) else {
                return Err(refuse("the pull request is gone"));
            };
            Some(HeldPayload::Merge(MergePayload::PullRequest {
                task,
                repo: repo.to_string(),
                pr,
                head: head.clone(),
                base: target.clone(),
                method: *method,
            }))
        }
        SettlePlan::PushBranch { head } => Some(HeldPayload::Push(PushPayload {
            task,
            remote: ORIGIN.to_owned(),
            branch: row.branch.clone(),
            head: head.clone(),
            expected_remote: loaded.delivery.pushed_head.clone(),
        })),
        SettlePlan::MergeInPlace { head } => Some(HeldPayload::Merge(MergePayload::InPlace {
            task,
            branch: row.branch.clone(),
            head: head.clone(),
            checkout: loaded.project.path.to_string_lossy().into_owned(),
            target_branch: target_branch.clone().unwrap_or_default(),
        })),
        SettlePlan::MarkDeliveredLocal { .. }
        | SettlePlan::CloseAnswered
        | SettlePlan::MarkDone
        | SettlePlan::Hold(_) => None,
    };
    let review_head = head.clone();
    card.review = svc
        .delivery()
        .read(move |conn| review_of(conn, task, &review_head))
        .await?;
    let local_delivery = matches!(inner, SettlePlan::MarkDeliveredLocal { .. });

    // 9. What settling does: hold for the user's approval, deliver locally, or land now.
    let act = if local_delivery {
        Act::Local
    } else {
        match (matches!(settle_plan, SettlePlan::Hold(_)), payload) {
            (true, Some(payload)) => Act::Hold(payload),
            (false, Some(payload)) => Act::LandNow(payload),
            (_, None) => return Err(refuse("there's nothing to settle under this delivery mode")),
        }
    };

    // Record: `reported → settling`, and for a hold its one payload. Only a task still reported is
    // settled; anything else changed meanwhile and is refused with the reason.
    let at = svc.delivery().clock().now();
    let title = row.title.clone();
    let asked_by = caller.agent;
    let parent = svc.delivery().parent();
    let record_head = head.clone();
    let record_branch = row.branch.clone();
    let facts_json = card.to_json(&record_head);
    let write = match &act {
        Act::Local => Write::Local,
        Act::Hold(payload) => Write::Hold(payload.clone()),
        Act::LandNow(_) => Write::Settling,
    };
    let recorded: Recorded = tasks
        .write(move |tx, notify| {
            let Some(now) = task_store::load(tx, task)? else {
                return Ok(Recorded::Refused(format!("{task} doesn't exist any more")));
            };
            if now.status != TaskStatus::Reported {
                return Ok(Recorded::Refused(format!(
                    "{task} is {} now, not reported, so it can't be settled",
                    now.status
                )));
            }
            let to = match next_for(task, now.status, TaskEvent::SettleStarted) {
                Ok(to) => to,
                Err(r) => return Ok(Recorded::Refused(r.reason)),
            };
            task_store::set_status(tx, task, now.status, to, at)?;
            let payload = match write {
                Write::Settling => return Ok(Recorded::Settling),
                Write::Local => {
                    // No remote to push to; delivered locally at once, no hold.
                    let fact = Fact::DeliveredLocal {
                        branch: record_branch,
                        head: record_head,
                    };
                    if !facts::landed(tx, notify, &*parent, task, &fact, at)? {
                        return Err(StoreError::Invalid {
                            column: "tasks.status",
                            message: format!("{task} couldn't be recorded as delivered"),
                        });
                    }
                    return Ok(Recorded::Delivered);
                }
                Write::Hold(payload) => payload,
            };
            // The payload is written once, in the transaction that moves the task to settling;
            // nothing rewrites it.
            let held = HeldService::insert(
                tx,
                &NewHeld {
                    payload,
                    choices: Vec::new(),
                    title,
                    task: Some(task),
                    asked_by: Some(asked_by),
                    at,
                },
            )?;
            let mut facts = facts_json;
            if let Some(map) = facts.as_object_mut() {
                map.insert("held".to_owned(), serde_json::json!(held));
            }
            task_store::event(
                tx,
                SETTLE_HELD_EVENT,
                task,
                Some(asked_by),
                &format!(
                    "{task} held for the user's approval at {}",
                    head_short(&facts)
                ),
                facts,
                at,
            )?;
            Ok(Recorded::Held(held))
        })
        .await
        .map_err(|e| failed(&e))?;
    drop(lock);

    let payload = match (recorded, act) {
        (Recorded::Refused(reason), _) => return Err(refuse(reason)),
        (Recorded::Held(held), Act::Hold(payload)) => {
            return Ok(SettleOutcome::Held {
                held_action: held,
                note: hold_note(&payload, task),
            });
        }
        (Recorded::Delivered, Act::Local) => {
            return Ok(SettleOutcome::Delivered {
                branch: row.branch.clone(),
                commit: head,
            });
        }
        // 10. Autonomy on, and the task is now settling: land now, with the performer's own
        // functions. Only this pairing ever lands.
        (Recorded::Settling, Act::LandNow(payload)) => payload,
        _ => {
            return Err(ToolFailure::Failed(format!(
                "settling {task} recorded something other than what it planned; nothing was landed"
            )));
        }
    };
    let landing = match &payload {
        HeldPayload::Merge(m @ MergePayload::PullRequest { .. }) => {
            land_pr(svc, m, Origin::Auto).await
        }
        HeldPayload::Merge(m @ MergePayload::InPlace { .. }) => {
            merge_in_place(svc, m, Origin::Auto).await
        }
        HeldPayload::Push(p) => push_branch(svc, p, Origin::Auto).await,
        _ => {
            return Err(ToolFailure::Failed(format!(
                "settling {task} planned something that isn't a merge or a push; nothing was landed"
            )));
        }
    }
    .map_err(|e| ToolFailure::Failed(e.to_string()))?;
    landing_outcome(task, landing)
}

/// What settling does after its checks.
enum Act {
    /// Hold for the user's approval with this one payload.
    Hold(HeldPayload),
    /// A branch-only project with no remote: delivered locally at once.
    Local,
    /// Autonomy on: land now with this payload.
    LandNow(HeldPayload),
}

/// What the recording transaction writes besides `reported → settling`.
enum Write {
    Hold(HeldPayload),
    Local,
    Settling,
}

/// What the recording transaction came to.
enum Recorded {
    Held(HeldActionId),
    Delivered,
    /// The task is settling, for autonomy to land now.
    Settling,
    /// Nothing was written: the reason.
    Refused(String),
}

/// A pull request GitHub already shows merged at exactly the reported and pushed head: the merge
/// is recorded (`reported → settling → landed`) instead of refused.
async fn record_merged(
    svc: &SettlingService,
    task: TaskId,
    pr: u64,
    head: CommitSha,
    merge_commit: CommitSha,
    base: String,
) -> Result<SettleOutcome, ToolFailure> {
    let at = svc.delivery().clock().now();
    let parent = svc.delivery().parent();
    let fact = Fact::MergedPr {
        pr,
        head: head.clone(),
        merge_commit,
        base,
    };
    let recorded: Result<(), String> = svc
        .tasks()
        .write(move |tx, notify| {
            let Some(now) = task_store::load(tx, task)? else {
                return Ok(Err(format!("{task} doesn't exist any more")));
            };
            if now.status != TaskStatus::Reported {
                return Ok(Err(format!(
                    "{task} is {} now, not reported, so it can't be settled",
                    now.status
                )));
            }
            let to = match next_for(task, now.status, TaskEvent::SettleStarted) {
                Ok(to) => to,
                Err(r) => return Ok(Err(r.reason)),
            };
            task_store::set_status(tx, task, now.status, to, at)?;
            if !facts::landed(tx, notify, &*parent, task, &fact, at)? {
                return Err(StoreError::Invalid {
                    column: "tasks.status",
                    message: format!("{task}'s merge couldn't be recorded"),
                });
            }
            Ok(Ok(()))
        })
        .await
        .map_err(|e| failed(&e))?;
    recorded.map_err(refuse)?;
    Ok(SettleOutcome::Merged { pr, commit: head })
}

/// What a landing tells the supervisor's tool call.
fn landing_outcome(task: TaskId, landing: Landing) -> Result<SettleOutcome, ToolFailure> {
    match landing {
        Landing::Landed(Fact::MergedPr { pr, head, .. }) => {
            Ok(SettleOutcome::Merged { pr, commit: head })
        }
        Landing::Landed(
            Fact::Delivered { branch, head } | Fact::DeliveredLocal { branch, head },
        )
        | Landing::Pushed { branch, head } => Ok(SettleOutcome::Delivered {
            branch,
            commit: head,
        }),
        Landing::Landed(Fact::DoneInFolder { folder }) => Ok(SettleOutcome::Done { folder }),
        Landing::Landed(Fact::InPlace { branch, head, .. }) => Ok(SettleOutcome::Landed {
            branch,
            commit: head,
        }),
        Landing::Unknown { reason } => Err(ToolFailure::Failed(format!(
            "{reason}. {task} stays settling; you're woken with settled once GitHub confirms the merge, or with settle_failed if it doesn't"
        ))),
        Landing::Refused { reason, .. } => Err(refuse(format!(
            "{reason}. Nothing was merged; the task is back to reported"
        ))),
    }
}

/// The note for the supervisor when the work waits for your approval.
fn hold_note(payload: &HeldPayload, task: TaskId) -> String {
    let what = match payload {
        HeldPayload::Merge(MergePayload::PullRequest { pr, head, .. }) => {
            format!("merge PR #{pr} at {}", head.short())
        }
        HeldPayload::Merge(MergePayload::InPlace {
            branch,
            head,
            target_branch,
            ..
        }) => format!(
            "merge {branch} at {} into {target_branch} in your checkout",
            head.short()
        ),
        HeldPayload::Push(p) => format!("push {} at {}", p.branch, p.head.short()),
        _ => format!("settle {task}"),
    };
    format!("Waiting for the user's approval: {what}.")
}

fn head_short(facts: &serde_json::Value) -> String {
    facts
        .get("head")
        .and_then(|h| h.as_str())
        .map(|h| h.chars().take(7).collect())
        .unwrap_or_default()
}

/// The stored head is the head everyone agrees on, or `settle_task` is refused with the SHAs named.
fn agreed(
    task: TaskId,
    head: &CommitSha,
    pushed: Option<&CommitSha>,
    reported: &CommitSha,
) -> Result<(), ToolFailure> {
    if head != reported {
        return Err(refuse(format!(
            "{task}'s branch is at {}, but its done report names {}; it must report done at the commit to settle",
            head.short(),
            reported.short()
        )));
    }
    if let Some(pushed) = pushed
        && pushed != head
    {
        return Err(refuse(format!(
            "{task}'s pull request is at {}, but the app last pushed {}; only the commit the app pushed can be merged",
            head.short(),
            pushed.short()
        )));
    }
    Ok(())
}

/// The first blocker of `task` that hasn't landed: landed, or closed as landed or answered,
/// counts.
fn unfinished_blocker(conn: &Connection, task: TaskId) -> Result<Option<TaskId>, StoreError> {
    for blocker in task_store::blockers_of(conn, task)? {
        let finished = match blocker.status {
            TaskStatus::Landed => true,
            TaskStatus::Closed => task_store::load(conn, blocker.task)?.is_some_and(|r| {
                matches!(r.outcome, Some(TaskOutcome::Landed | TaskOutcome::Answered))
            }),
            _ => false,
        };
        if !finished {
            return Ok(Some(blocker.task));
        }
    }
    Ok(None)
}

/// The Careful line on the card: the verdict and risk of the review whose pass is recorded for
/// exactly `head` (the head on its `task.review_passed` event, the same fact the gate reads), so
/// the card never drops a pass the gate accepts. Without such a pass, the latest verdict of a
/// review started at exactly `head`; a review of an earlier commit says nothing about this one.
fn review_of(
    conn: &Connection,
    task: TaskId,
    head: &CommitSha,
) -> Result<Option<(String, String)>, StoreError> {
    if passed_for(conn, task, head)? {
        let event =
            task_store::last_event(conn, task, crate::verification::review::REVIEW_PASSED_EVENT)?;
        let report = event
            .as_ref()
            .and_then(|(_, p)| p.get("report").cloned())
            .and_then(|r| serde_json::from_value::<ReportId>(r).ok());
        let from_report = match report {
            Some(report) => conn
                .query_row(
                    "SELECT review_verdict, COALESCE(review_risk, '') FROM reports
                     WHERE id = ?1 AND review_verdict IS NOT NULL",
                    params![report],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?,
            None => None,
        };
        let risk = event
            .as_ref()
            .and_then(|(_, p)| p.get("risk").and_then(serde_json::Value::as_str))
            .unwrap_or_default()
            .to_owned();
        return Ok(Some(
            from_report.unwrap_or_else(|| ("pass".to_owned(), risk)),
        ));
    }
    Ok(conn
        .query_row(
            "SELECT r.review_verdict, COALESCE(r.review_risk, '') FROM reports AS r
             JOIN tasks AS t ON t.id = r.task_id
             WHERE t.builds_on = ?1 AND t.kind = 'review' AND t.base_commit = ?2
               AND r.review_verdict IS NOT NULL
             ORDER BY r.seq DESC LIMIT 1",
            params![task, head.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?)
}

/// What the checkout told settling.
#[derive(Clone, Debug)]
struct LocalFacts {
    /// The task's own commit messages up to the reported head, or why git couldn't give them.
    messages: Result<Vec<String>, String>,
    tip: Option<CommitSha>,
    has_origin: bool,
    clean: Option<bool>,
    current: Option<String>,
    stat: Option<qm_git::DiffStat>,
}

/// The facts an approval card shows, recorded in the `settle.held` event.
#[derive(Clone, Debug, Default)]
struct CardFacts {
    additions: u64,
    deletions: u64,
    checks: Option<(u32, u32)>,
    review: Option<(String, String)>,
}

impl CardFacts {
    fn to_json(&self, head: &CommitSha) -> serde_json::Value {
        serde_json::json!({
            "head": head,
            "additions": self.additions,
            "deletions": self.deletions,
            "checks_passed": self.checks.map(|c| c.0),
            "checks_total": self.checks.map(|c| c.1),
            "review": self.review.as_ref().map(|(verdict, risk)| serde_json::json!({
                "verdict": verdict, "risk": risk,
            })),
        })
    }
}
