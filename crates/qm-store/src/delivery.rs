//! Delivery facts on a task: the commit the app last pushed, its checks as last read, and where
//! landed work went. One writer: the core's delivery and settling code. Every function runs inside
//! the caller's transaction.

use crate::StoreError;
use crate::held::{HeldPayload, MergePayload, load_held};
use crate::tasks;
use qm_types::domain::{DeliveryMode, HeldKind, TaskOutcome, TaskStatus};
use qm_types::ids::{CommitSha, HeldActionId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// `checks_state.verdict` values.
pub mod verdict {
    pub const PENDING: &str = "pending";
    pub const PASSED: &str = "passed";
    pub const FAILED: &str = "failed";
    pub const MISSING_REQUIRED: &str = "missing_required";
    pub const NO_CHECKS: &str = "no_checks";
}

/// The pushed head's checks as last read (`tasks.checks_state`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksState {
    pub head: CommitSha,
    /// The verdict won't change any more for this head (its one message was sent).
    #[serde(rename = "final")]
    pub is_final: bool,
    /// One of [`verdict`]'s values.
    pub verdict: String,
    /// Failing (or missing required) check names.
    #[serde(default)]
    pub failing: Vec<String>,
    #[serde(default)]
    pub passed: u32,
    #[serde(default)]
    pub total: u32,
    /// Reruns asked for on this head; a rerun's result wakes the crewmate again.
    #[serde(default)]
    pub reruns: u32,
    /// When this state was written, ms since the epoch.
    pub at: i64,
    /// When the head was pushed, ms since the epoch (the "no checks" grace runs from here).
    #[serde(default)]
    pub pushed_at: i64,
    /// When no check was pending any more, ms (the missing-required grace runs from here).
    #[serde(default)]
    pub quiet_since: Option<i64>,
    /// The branch whose required checks count: the PR's target, or the pushed task branch.
    #[serde(default)]
    pub required_branch: Option<String>,
}

impl ChecksState {
    /// A fresh head just pushed: pending, nothing read yet.
    pub fn pushed(head: CommitSha, required_branch: &str, at_ms: i64) -> Self {
        Self {
            head,
            is_final: false,
            verdict: verdict::PENDING.to_owned(),
            failing: Vec::new(),
            passed: 0,
            total: 0,
            reruns: 0,
            at: at_ms,
            pushed_at: at_ms,
            quiet_since: None,
            required_branch: Some(required_branch.to_owned()),
        }
    }
}

/// A task's delivery columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryRow {
    pub pr_number: Option<u64>,
    pub pushed_head: Option<CommitSha>,
    pub checks: Option<ChecksState>,
    pub landed_commit: Option<CommitSha>,
    pub landed_into: Option<String>,
}

fn sha(column: &'static str, s: Option<String>) -> Result<Option<CommitSha>, StoreError> {
    s.map(|s| {
        s.parse::<CommitSha>().map_err(|e| StoreError::Invalid {
            column,
            message: e.to_string(),
        })
    })
    .transpose()
}

fn json<T: Serialize>(v: &T) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json {
        column: "tasks.checks_state",
        source,
    })
}

/// The task's delivery columns; `None` if the task doesn't exist.
pub fn load(conn: &Connection, task: TaskId) -> Result<Option<DeliveryRow>, StoreError> {
    let raw = conn
        .query_row(
            "SELECT pr_number, pushed_head, checks_state, landed_commit, landed_into
             FROM tasks WHERE id = ?1",
            params![task],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((pr, pushed, checks, landed, into)) = raw else {
        return Ok(None);
    };
    Ok(Some(DeliveryRow {
        pr_number: pr.and_then(|n| u64::try_from(n).ok()),
        pushed_head: sha("tasks.pushed_head", pushed)?,
        checks: checks
            .map(|c| {
                serde_json::from_str(&c).map_err(|source| StoreError::Json {
                    column: "tasks.checks_state",
                    source,
                })
            })
            .transpose()?,
        landed_commit: sha("tasks.landed_commit", landed)?,
        landed_into: into,
    }))
}

/// Records a push: `pushed_head` and `head_commit`, the pull request's number when there is one,
/// and the new head's pending checks.
pub fn record_push(
    conn: &Connection,
    task: TaskId,
    pr: Option<u64>,
    head: &CommitSha,
    checks: &ChecksState,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET pushed_head = ?2, head_commit = ?2, checks_state = ?3,
             pr_number = COALESCE(?4, pr_number)
         WHERE id = ?1",
        params![
            task,
            head.as_str(),
            json(checks)?,
            pr.and_then(|n| i64::try_from(n).ok())
        ],
    )?;
    Ok(())
}

/// Records the pull request's number (a PR found or created without a new push).
pub fn set_pr_number(conn: &Connection, task: TaskId, pr: u64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET pr_number = ?2 WHERE id = ?1",
        params![task, i64::try_from(pr).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

/// Writes the checks as last read, only while `head` is still the pushed head. Returns whether
/// it was written.
pub fn set_checks(
    conn: &Connection,
    task: TaskId,
    checks: &ChecksState,
) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "UPDATE tasks SET checks_state = ?2 WHERE id = ?1 AND pushed_head = ?3",
        params![task, json(checks)?, checks.head.as_str()],
    )? == 1)
}

/// Records where landed work went.
pub fn set_landed(
    conn: &Connection,
    task: TaskId,
    commit: Option<&CommitSha>,
    into: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET landed_commit = ?2, landed_into = ?3 WHERE id = ?1",
        params![task, commit.map(CommitSha::as_str), into],
    )?;
    Ok(())
}

/// Where the task's landed work went, if it landed.
pub fn landed_into(conn: &Connection, task: TaskId) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT landed_into FROM tasks WHERE id = ?1",
            params![task],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// The task's pending settle holds (`merge` and `push`), oldest first.
pub fn pending_settle_holds(
    conn: &Connection,
    task: TaskId,
) -> Result<Vec<HeldActionId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id FROM held_actions
         WHERE task_id = ?1 AND status = 'pending' AND kind IN ('merge', 'push')
         ORDER BY seq",
    )?;
    Ok(stmt
        .query_map(params![task], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Settle holds of the task that were claimed by an answer but never recorded (the app stopped
/// while carrying one out).
pub fn unfinished_settle_holds(
    conn: &Connection,
    task: TaskId,
) -> Result<Vec<HeldActionId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id FROM held_actions
         WHERE task_id = ?1 AND status IN ('approved', 'rejected') AND answer IS NULL
           AND kind IN ('merge', 'push')
         ORDER BY seq",
    )?;
    Ok(stmt
        .query_map(params![task], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Tasks whose pushed head's checks aren't final yet (the poller re-arms them at startup).
pub fn open_checks(conn: &Connection) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks
         WHERE status <> 'closed' AND pushed_head IS NOT NULL AND checks_state IS NOT NULL
           AND json_extract(checks_state, '$.final') = 0
         ORDER BY seq",
    )?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every open direct-PR task with a pull request: the ones the outside watch reads.
pub fn with_pull_request(conn: &Connection) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks
         WHERE status NOT IN ('closed', 'landed', 'failed', 'stopped', 'discarded')
           AND delivery_mode = 'direct_pr' AND pr_number IS NOT NULL
         ORDER BY seq",
    )?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The head a pending settle hold is pinned to, and its kind.
fn hold_head(
    conn: &Connection,
    held: HeldActionId,
) -> Result<Option<(HeldKind, CommitSha, Option<String>)>, StoreError> {
    let row = load_held(conn, held)?;
    Ok(match row.payload {
        HeldPayload::Merge(MergePayload::PullRequest { head, .. }) => {
            Some((HeldKind::Merge, head, None))
        }
        HeldPayload::Merge(MergePayload::InPlace {
            head,
            target_branch,
            ..
        }) => Some((HeldKind::Merge, head, Some(target_branch))),
        HeldPayload::Push(p) => Some((HeldKind::Push, p.head, None)),
        _ => None,
    })
}

/// The task's delivery line for the context block and task lists;
/// `None` when delivery has nothing to say yet.
pub fn task_line(conn: &Connection, task: TaskId) -> Result<Option<String>, StoreError> {
    let Some(row) = tasks::load(conn, task)? else {
        return Ok(None);
    };
    let Some(d) = load(conn, task)? else {
        return Ok(None);
    };
    let landed = row.status == TaskStatus::Landed
        || (row.status == TaskStatus::Closed && row.outcome == Some(TaskOutcome::Landed));
    if landed {
        return Ok(Some(match row.delivery {
            DeliveryMode::DirectPr => match d.pr_number {
                Some(pr) => format!("merged #{pr}"),
                None => "merged".to_owned(),
            },
            DeliveryMode::BranchOnly if d.pushed_head.is_some() => {
                "branch pushed · delivered".to_owned()
            }
            DeliveryMode::BranchOnly => "branch delivered · kept in the workspace".to_owned(),
            DeliveryMode::BuildInPlace => format!(
                "landed in {}",
                d.landed_into.as_deref().unwrap_or("your checkout")
            ),
            DeliveryMode::Folder => "done · its changes are in the folder".to_owned(),
        }));
    }
    if row.status == TaskStatus::Closed {
        return Ok(None);
    }
    let checks = d
        .checks
        .as_ref()
        .filter(|c| Some(&c.head) == d.pushed_head.as_ref());
    if row.status == TaskStatus::Settling
        && let Some(held) = pending_settle_holds(conn, task)?.last()
        && let Some((kind, head, target)) = hold_head(conn, *held)?
    {
        let short = head.short();
        return Ok(Some(match (kind, target, d.pr_number) {
            (HeldKind::Push, _, _) => format!("branch ready · held for your approval @{short}"),
            (_, Some(target), _) => {
                format!("ready to land in {target} · held for your approval @{short}")
            }
            (_, None, Some(pr)) => match checks {
                Some(c) if c.verdict == verdict::PASSED => format!(
                    "PR #{pr} ready · {}/{} · held for your approval @{short}",
                    c.passed, c.total
                ),
                _ => format!("PR #{pr} ready · held for your approval @{short}"),
            },
            (_, None, None) => format!("held for your approval @{short}"),
        }));
    }
    let Some(c) = checks else {
        return Ok(d.pr_number.map(|pr| format!("PR #{pr} open")));
    };
    let short = c.head.short();
    let pending = !c.is_final;
    Ok(Some(match (row.delivery, d.pr_number) {
        (DeliveryMode::DirectPr, Some(pr)) => {
            if pending {
                format!("PR #{pr} open · checks running on {short}")
            } else if c.verdict == verdict::PASSED {
                format!("PR #{pr} ready · {}/{} on {short}", c.passed, c.total)
            } else if c.verdict == verdict::NO_CHECKS {
                format!("PR #{pr} open · no checks ran on {short}")
            } else {
                format!(
                    "PR #{pr} open · checks failed on {short}: {}",
                    c.failing.join(", ")
                )
            }
        }
        _ => {
            if pending {
                format!("branch pushed · checks running on {short}")
            } else if c.verdict == verdict::PASSED || c.verdict == verdict::NO_CHECKS {
                format!("branch pushed · checks done on {short}")
            } else {
                format!(
                    "branch pushed · checks failed on {short}: {}",
                    c.failing.join(", ")
                )
            }
        }
    }))
}
