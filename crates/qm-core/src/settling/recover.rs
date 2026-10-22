//! Startup, and merges whose outcome GitHub hasn't confirmed yet.
//!
//! Every task left in `settling` is concluded. `run` is awaited before the seat runs and before
//! any card can be answered, so every claim it finds is the last run's and no performer of this
//! run can be carrying one out: a claim with nothing sent to GitHub is given back (the card is
//! pending again), and a claimed card is never superseded here.
//!
//! A merge that was requested and never concluded, or one GitHub answered late or unclearly, is
//! read back from GitHub (`watch_after`) under the task's lock until GitHub settles it, and is
//! **never** merged again without that read-back. GitHub not answering is never taken for "not
//! merged". At startup that includes a merge whose task left `settling` before GitHub confirmed it
//! (stopped while it waited, say), whatever the task's status now.

use super::facts::{self, Fact};
use super::land_pr::moved;
use super::performers::YOU_APPROVED_MERGE;
use super::push::branch_checks_final;
use super::{
    MERGE_CONCLUDED_EVENT, MERGE_REQUESTED_EVENT, Origin, SETTLE_REFUSED_EVENT, SettlingService,
    refuse_in,
};
use crate::delivery::checks::stored_verdict;
use crate::inbox::{Inbox, Notify};
use crate::tasks::lifecycle::{TaskEvent, next_for};
use crate::tasks::new_wake;
use jiff::{SignedDuration, Timestamp};
use qm_github::{PrState, RepoRef};
use qm_store::StoreError;
use qm_store::delivery as delivery_store;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::time::from_millis;
use qm_types::api::HeldAnswer;
use qm_types::domain::{DeliveryMode, TaskStatus, WakeKind};
use qm_types::ids::{AgentId, CommitSha, EventId, HeldActionId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use std::time::Duration;

/// How long until a merge whose outcome isn't known yet is read again.
pub(crate) const RETRY_EVERY: Duration = Duration::from_secs(60);
/// How long a pull request may stay open at the approved head after its merge was requested
/// before it counts as not merged (GitHub can finish a merge late).
const OPEN_FOR_AT_MOST: SignedDuration = SignedDuration::from_secs(15 * 60);

/// One requested merge not concluded yet, from its `settle.merge_requested` event.
#[derive(Clone, Debug)]
pub(crate) struct MergeWatch {
    pub requested: EventId,
    pub requested_at: Timestamp,
    pub repo: String,
    pub pr: u64,
    pub head: CommitSha,
    pub base: String,
    pub held: Option<HeldActionId>,
}

/// What the last run left for one settling task.
#[derive(Clone, Debug)]
enum Left {
    /// A merge request with no conclusion after it.
    MergeRequested(MergeWatch),
    /// A pending approval: it stays pending.
    PendingHold,
    /// A branch-only push waiting on its checks.
    AwaitingChecks,
    /// An approval claimed by an answer whose performer never finished.
    Claimed,
    /// Nothing that can finish by itself.
    Nothing,
}

/// Whether the merge request `requested` (of `head`) has its conclusion: the merge recorded, the
/// settle refused at that head (by the merge or its read-back), or the read-back's own conclusion
/// after the task had left settling. A refusal written for another reason (no head, or another
/// head) doesn't conclude it.
fn concluded(
    conn: &Connection,
    task: TaskId,
    requested: EventId,
    head: &str,
) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events
             WHERE task_id = ?1
               AND seq > COALESCE((SELECT seq FROM events WHERE id = ?2), 0)
               AND (kind IN (?3, ?4, ?5)
                    OR (kind = ?6 AND json_extract(payload, '$.head') = ?7)))",
        params![
            task,
            requested,
            facts::PR_MERGED_EVENT,
            facts::OUTCOME_MERGED,
            MERGE_CONCLUDED_EVENT,
            SETTLE_REFUSED_EVENT,
            head
        ],
        |r| r.get(0),
    )?)
}

/// The commit of `task`'s merge that GitHub hasn't confirmed or refused yet, if any. While it's
/// there nothing may move the task's head or level (`pr_open`, `set_verification`): the merge
/// could still land at it.
pub(crate) fn merge_unconfirmed(
    conn: &Connection,
    task: TaskId,
) -> Result<Option<CommitSha>, StoreError> {
    Ok(unconcluded_merge(conn, task)?.map(|w| w.head))
}

/// The refusal for something that would move a task whose merge of `head` is unconfirmed.
pub(crate) fn merge_waiting(head: &CommitSha) -> String {
    format!(
        "a merge of {} is waiting for GitHub to confirm; try again once it's settled (you're woken either way)",
        head.short()
    )
}

/// The task's merge request that nothing concluded yet, if any.
fn unconcluded_merge(conn: &Connection, task: TaskId) -> Result<Option<MergeWatch>, StoreError> {
    let Some((requested, payload)) = task_store::last_event(conn, task, MERGE_REQUESTED_EVENT)?
    else {
        return Ok(None);
    };
    let Some(requested_head) = payload.get("head").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    if concluded(conn, task, requested, requested_head)? {
        return Ok(None);
    }
    let get = |k: &str| {
        payload
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let (Some(repo), Some(pr), Some(head), Some(base)) = (
        get("repo"),
        payload.get("pr").and_then(serde_json::Value::as_u64),
        get("head").and_then(|h| h.parse().ok()),
        get("base"),
    ) else {
        return Ok(None);
    };
    let at: i64 = conn.query_row(
        "SELECT at FROM events WHERE id = ?1",
        params![requested],
        |r| r.get(0),
    )?;
    Ok(Some(MergeWatch {
        requested,
        requested_at: from_millis("events.at", at)?,
        repo,
        pr,
        head,
        base,
        held: payload
            .get("held")
            .cloned()
            .and_then(|h| serde_json::from_value::<HeldActionId>(h).ok()),
    }))
}

fn what_was_left(conn: &Connection, task: TaskId) -> Result<Left, StoreError> {
    if let Some(watch) = unconcluded_merge(conn, task)? {
        return Ok(Left::MergeRequested(watch));
    }
    if !delivery_store::pending_settle_holds(conn, task)?.is_empty() {
        return Ok(Left::PendingHold);
    }
    let row = task_store::load(conn, task)?;
    let d = delivery_store::load(conn, task)?.unwrap_or_default();
    // Only a push of the head being settled counts: an older push's checks say nothing about it.
    if row.is_some_and(|r| {
        r.delivery == DeliveryMode::BranchOnly
            && r.head_commit.is_some()
            && r.head_commit == d.pushed_head
    }) && d.checks.as_ref().map(|c| &c.head) == d.pushed_head.as_ref()
    {
        return Ok(Left::AwaitingChecks);
    }
    if !delivery_store::unfinished_settle_holds(conn, task)?.is_empty() {
        return Ok(Left::Claimed);
    }
    Ok(Left::Nothing)
}

/// Concludes every task in `settling` (awaited at startup, before the seat runs and before any
/// card can be answered).
pub(crate) async fn run(svc: &SettlingService) {
    let settling = match svc
        .tasks()
        .db()
        .call(|conn| task_store::with_status(conn, TaskStatus::Settling))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "couldn't read the tasks left settling");
            return;
        }
    };
    for row in settling {
        if let Err(e) = conclude(svc, row.id).await {
            tracing::warn!(task = %row.id, error = %e, "couldn't conclude a settle left by the last run");
        }
    }
    // A merge GitHub hadn't confirmed when its task left settling (stopped, say) is read back
    // too, whatever the task's status now, so a merge that landed late is still recorded.
    let at = svc.delivery().clock().now();
    let elsewhere = match svc
        .tasks()
        .db()
        .call(move |conn| unconfirmed_outside_settling(conn, at))
        .await
    {
        Ok(watches) => watches,
        Err(e) => {
            tracing::error!(error = %e, "couldn't read the merges left unconfirmed");
            return;
        }
    };
    for (task, watch) in elsewhere {
        watch_after(svc, task, watch, Duration::ZERO);
    }
}

/// Every task not in `settling` whose merge request is still unconfirmed
/// ([`merge_unconfirmed`]), with that request. Settling tasks are [`conclude`]'s.
fn unconfirmed_outside_settling(
    conn: &mut Connection,
    at: Timestamp,
) -> Result<Vec<(TaskId, MergeWatch)>, StoreError> {
    let tasks: Vec<TaskId> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT e.task_id FROM events e JOIN tasks t ON t.id = e.task_id
             WHERE e.kind = ?1 AND t.status <> ?2
             ORDER BY e.task_id",
        )?;
        stmt.query_map(params![MERGE_REQUESTED_EVENT, TaskStatus::Settling], |r| {
            r.get(0)
        })?
        .collect::<Result<_, _>>()?
    };
    let mut out = Vec::new();
    for task in tasks {
        if let Some(watch) = unconcluded_merge(conn, task)? {
            record_requested_claim(conn, task, &watch, at)?;
            out.push((task, watch));
        }
    }
    Ok(out)
}

/// The card whose Approve the last run carried as far as `watch`'s merge request, recorded as
/// approved: the app stopped before writing its answer, and the Approve was carried out, so the
/// card is never offered again; the read-back then settles what GitHub did. Startup only, when no
/// performer of this run can be carrying the card.
fn record_requested_claim(
    conn: &Connection,
    task: TaskId,
    watch: &MergeWatch,
    at: Timestamp,
) -> Result<(), StoreError> {
    let Some(held) = watch.held else {
        return Ok(());
    };
    let answer = serde_json::to_value(HeldAnswer::Approve).map_err(|source| StoreError::Json {
        column: "held_actions.answer",
        source,
    })?;
    let asked_by: Option<Option<AgentId>> = conn
        .query_row(
            "SELECT asked_by FROM held_actions
             WHERE id = ?1 AND status = 'approved' AND answer IS NULL",
            params![held],
            |r| r.get(0),
        )
        .optional()?;
    let Some(asked_by) = asked_by else {
        return Ok(());
    };
    conn.execute(
        "UPDATE held_actions SET answer = ?2 WHERE id = ?1 AND status = 'approved' AND answer IS NULL",
        params![held, answer.to_string()],
    )?;
    let short = watch.head.short();
    let note = format!(
        "The app stopped after asking GitHub to merge PR #{} at {short}; what GitHub did is read back from GitHub.",
        watch.pr
    );
    task_store::event(
        conn,
        YOU_APPROVED_MERGE,
        task,
        asked_by,
        &format!("you approved the merge of {task} at {short}"),
        serde_json::json!({ "held": held, "answer": answer, "note": note }),
        at,
    )
    .map(drop)
}

/// What [`conclude`] does after its transaction.
enum Then {
    Nothing,
    Arm(CommitSha),
    Watch(MergeWatch),
}

async fn conclude(svc: &SettlingService, task: TaskId) -> Result<(), StoreError> {
    // One settle of a task at a time, recovery included.
    let lock = svc.delivery().locks().lock(task).await;
    let parent = svc.delivery().parent();
    let at = svc.delivery().clock().now();
    let then = svc
        .tasks()
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(Then::Nothing);
            };
            if row.status != TaskStatus::Settling {
                return Ok(Then::Nothing);
            }
            match what_was_left(tx, task)? {
                Left::PendingHold => Ok(Then::Nothing),
                Left::MergeRequested(watch) => {
                    record_requested_claim(tx, task, &watch, at)?;
                    Ok(Then::Watch(watch))
                }
                Left::AwaitingChecks => {
                    let d = delivery_store::load(tx, task)?.unwrap_or_default();
                    let (Some(head), Some(state)) = (d.pushed_head, d.checks) else {
                        return Ok(Then::Nothing);
                    };
                    if !state.is_final {
                        return Ok(Then::Arm(head));
                    }
                    // The verdict was recorded but its outcome wasn't: apply it now.
                    let verdict = stored_verdict(&state);
                    branch_checks_final(tx, notify, &*parent, task, &head, &verdict, at)?;
                    Ok(Then::Nothing)
                }
                Left::Claimed => {
                    // The last run claimed the card and stopped before its performer finished,
                    // with nothing sent to GitHub: the card is given back, pending again, for
                    // the user to answer. Never superseded.
                    for held in delivery_store::unfinished_settle_holds(tx, task)? {
                        tx.execute(
                            "UPDATE held_actions SET status = 'pending', decided_at = NULL
                             WHERE id = ?1 AND status IN ('approved', 'rejected')
                               AND answer IS NULL",
                            params![held],
                        )?;
                    }
                    Ok(Then::Nothing)
                }
                Left::Nothing => {
                    // A settle the last run never finished: nothing left the Mac, so the task
                    // goes back to reported.
                    if let Some(head) = row.head_commit {
                        let text = format!(
                            "The app stopped while settling {task}, before anything was merged or pushed. It's back to reported; settle again."
                        );
                        refuse_in(
                            tx,
                            notify,
                            task,
                            &head,
                            "the app stopped while settling it",
                            Origin::Auto,
                            Some(text),
                            at,
                        )?;
                    }
                    Ok(Then::Nothing)
                }
            }
        })
        .await?;
    drop(lock);
    match then {
        Then::Nothing => {}
        Then::Arm(head) => svc.delivery().arm(task, head),
        Then::Watch(watch) => watch_after(svc, task, watch, Duration::ZERO),
    }
    Ok(())
}

fn key(task: TaskId) -> String {
    format!("settle-readback:{task}")
}

/// Reads `watch` back from GitHub after `delay`, and again every [`RETRY_EVERY`] until GitHub
/// settles it (replacing any read-back of the task already waiting).
pub(crate) fn watch_after(svc: &SettlingService, task: TaskId, watch: MergeWatch, delay: Duration) {
    let again = svc.clone();
    svc.delivery().scheduler().after(
        key(task),
        delay,
        Box::new(move || retry(again, task, watch)),
    );
}

/// A read-back run by the scheduler (boxed, so the retry's type doesn't refer to itself).
fn retry(
    svc: SettlingService,
    task: TaskId,
    watch: MergeWatch,
) -> qm_types::BoxFuture<'static, ()> {
    Box::pin(async move {
        if let Err(e) = read_back(&svc, task, watch).await {
            tracing::warn!(%task, error = %e, "couldn't conclude a merge whose outcome wasn't known");
        }
    })
}

/// A requested merge read back from GitHub, under the task's lock, until it has its conclusion. It
/// never stops silently.
///
/// - GitHub unreadable, merged without its merge commit yet, or still open at the head within
///   [`OPEN_FOR_AT_MOST`] of the request: read again later.
/// - Merged at the requested head: the fact. A task that left settling meanwhile still gets it
///   when it's reported; otherwise the merge is recorded as the request's conclusion and the
///   supervisor is woken.
/// - Anything else: not merged. A settling task goes back to reported; either way the request is
///   concluded and the supervisor is woken.
///
/// Nothing is ever merged again.
#[allow(clippy::too_many_lines)]
async fn read_back(
    svc: &SettlingService,
    task: TaskId,
    watch: MergeWatch,
) -> Result<(), StoreError> {
    let _lock = svc.delivery().locks().lock(task).await;
    // Only while this very request is still the task's unconcluded one, whatever its status.
    let current = svc
        .tasks()
        .db()
        .call(move |conn| unconcluded_merge(conn, task))
        .await?;
    if current.is_none_or(|c| c.requested != watch.requested) {
        return Ok(());
    }
    let Some(repo_ref) = RepoRef::parse(&watch.repo) else {
        return Ok(());
    };
    let pr = watch.pr;
    let pull = match svc.delivery().github().pull(&repo_ref, pr).await {
        Ok(pull) => pull,
        Err(e) => {
            tracing::info!(%task, error = %e.plain(), "GitHub couldn't be read; the merge's outcome is read again later");
            watch_after(svc, task, watch, RETRY_EVERY);
            return Ok(());
        }
    };
    let at = svc.delivery().clock().now();
    let head = watch.head.clone();
    let at_head = pull.head_sha == head;
    let requested = watch.requested;
    if at_head && pull.merged {
        let Some(merge_commit) = pull.merge_commit.clone() else {
            watch_after(svc, task, watch, RETRY_EVERY);
            return Ok(());
        };
        let parent = svc.delivery().parent();
        let fact = Fact::MergedPr {
            pr,
            head: head.clone(),
            merge_commit,
            base: watch.base,
        };
        return svc
            .tasks()
            .write(move |tx, notify| {
                let Some(row) = task_store::load(tx, task)? else {
                    return Ok(());
                };
                if row.status == TaskStatus::Reported
                    && let Ok(to) = next_for(task, row.status, TaskEvent::SettleStarted)
                {
                    task_store::set_status(tx, task, row.status, to, at)?;
                }
                if facts::landed(tx, notify, &*parent, task, &fact, at)? {
                    return Ok(());
                }
                // The task moved on (it's no longer settling or reported): the merge is still a
                // fact on GitHub, recorded as the request's conclusion.
                let text = format!(
                    "GitHub confirmed the merge of {task}'s PR #{pr} at {}, after {task} had left settling (it's {} now). The merge happened; nothing else about {task} was changed.",
                    head.short(),
                    row.status
                );
                conclude_outside(
                    tx,
                    notify,
                    &row,
                    requested,
                    &head,
                    true,
                    &text,
                    WakeKind::Settled,
                    at,
                )
            })
            .await;
    }
    let open = pull.state == PrState::Open && !pull.merged;
    if at_head && open && at.duration_since(watch.requested_at) < OPEN_FOR_AT_MOST {
        watch_after(svc, task, watch, RETRY_EVERY);
        return Ok(());
    }
    let reason = if pull.merged {
        format!(
            "PR #{pr} was merged at {}, not the approved {}; check it on GitHub",
            pull.head_sha.short(),
            head.short()
        )
    } else if !at_head {
        moved(&head, &pull.head_sha)
    } else if open {
        format!(
            "PR #{pr} isn't merged: GitHub still shows it open at {}",
            head.short()
        )
    } else {
        format!("PR #{pr} isn't merged: it was closed")
    };
    let origin = watch
        .held
        .map_or(Origin::Auto, |held| Origin::Approval { held });
    svc.tasks()
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(());
            };
            if row.status == TaskStatus::Settling {
                let text = format!(
                    "GitHub never confirmed the merge of {task}: {reason}. Nothing was merged by the app; it's back to reported. Settle again."
                );
                return refuse_in(tx, notify, task, &head, &reason, origin, Some(text), at)
                    .map(drop);
            }
            let text = format!(
                "GitHub never confirmed the merge of {task} at {}: {reason}. Nothing was merged by the app; {task} is {} and stays so.",
                head.short(),
                row.status
            );
            conclude_outside(
                tx,
                notify,
                &row,
                requested,
                &head,
                false,
                &text,
                WakeKind::SettleFailed,
                at,
            )
        })
        .await
}

/// A merge request concluded after its task left settling: the conclusion on record and the
/// supervisor woken, so the request never ends silently.
#[allow(clippy::too_many_arguments)]
fn conclude_outside(
    tx: &Connection,
    notify: &mut Notify,
    row: &TaskRow,
    requested: EventId,
    head: &CommitSha,
    merged: bool,
    text: &str,
    wake: WakeKind,
    at: Timestamp,
) -> Result<(), StoreError> {
    let task = row.id;
    let short = if merged {
        format!("{task}'s merge confirmed late")
    } else {
        format!("{task}'s merge didn't happen")
    };
    let event = task_store::event(
        tx,
        MERGE_CONCLUDED_EVENT,
        task,
        None,
        text,
        serde_json::json!({ "requested": requested, "head": head, "merged": merged }),
        at,
    )?;
    Inbox::post(
        tx,
        &new_wake(
            row.supervisor,
            wake,
            task,
            format!("merge_concluded:{task}:{}:{event}", head.short()),
            text,
            &short,
            serde_json::json!({ "head": head, "merged": merged }),
            at,
        ),
        notify,
    )?;
    Ok(())
}
