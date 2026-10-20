//! A task's verification facts.
//!
//! The columns of migration 0005 (`repair_pending`, `repair_held`, `review_passed_at`,
//! `validation_pending`) and 0001's counters (`repair_attempts`, `review_rounds`,
//! `flake_rerun_used`), each with one writer: the core's `verification` and `decisions` modules.
//! Everything here is a plain function over the caller's connection, so a fact and the wake it
//! causes are written in one transaction. `qm-store` doesn't depend on `qm-policy`: the repair
//! counters travel as a plain [`RepairRow`] the core maps to `RepairState`.

use crate::StoreError;
use crate::reports::ReportRow;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{TaskStatus, VerificationLevel};
use qm_types::ids::{AgentId, HeldActionId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// The repair counters as stored: `repair_pending` (JSON, `{"declared":[…],"awaiting":[…]}`),
/// `repair_attempts`, `flake_rerun_used` and `repair_held`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairRow {
    pub pending: String,
    pub attempts: u8,
    pub rerun_used: bool,
    pub held: bool,
}

/// Every verification fact of one task, as one row reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationFacts {
    pub repair: RepairRow,
    pub review_rounds: u8,
    pub review_passed_at: Option<Timestamp>,
    pub validation_pending: bool,
}

fn small(column: &'static str, n: i64) -> Result<u8, StoreError> {
    u8::try_from(n.clamp(0, i64::from(u8::MAX))).map_err(|_| StoreError::Invalid {
        column,
        message: format!("{n} is out of range"),
    })
}

/// The task's verification facts; `None` if it doesn't exist.
pub fn facts(conn: &Connection, task: TaskId) -> Result<Option<VerificationFacts>, StoreError> {
    let raw = conn
        .query_row(
            "SELECT repair_pending, repair_attempts, flake_rerun_used, repair_held, review_rounds,
                    review_passed_at, validation_pending
             FROM tasks WHERE id = ?1",
            params![task],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((pending, attempts, rerun_used, held, rounds, passed, validation)) = raw else {
        return Ok(None);
    };
    Ok(Some(VerificationFacts {
        repair: RepairRow {
            pending,
            attempts: small("tasks.repair_attempts", attempts)?,
            rerun_used,
            held,
        },
        review_rounds: small("tasks.review_rounds", rounds)?,
        review_passed_at: passed
            .map(|ms| from_millis("tasks.review_passed_at", ms))
            .transpose()?,
        validation_pending: validation,
    }))
}

/// Saves the repair counters, in the transaction of the act that changed them.
pub fn save_repair(conn: &Connection, task: TaskId, repair: &RepairRow) -> Result<(), StoreError> {
    serde_json::from_str::<serde_json::Value>(&repair.pending).map_err(|source| {
        StoreError::Json {
            column: "tasks.repair_pending",
            source,
        }
    })?;
    conn.execute(
        "UPDATE tasks SET repair_pending = ?2, repair_attempts = ?3, flake_rerun_used = ?4,
             repair_held = ?5
         WHERE id = ?1",
        params![
            task,
            repair.pending,
            i64::from(repair.attempts),
            repair.rerun_used,
            repair.held
        ],
    )?;
    Ok(())
}

/// Your turn's level for a running task (`set_verification`): the level, with `user` as its source.
pub fn set_level(
    conn: &Connection,
    task: TaskId,
    level: VerificationLevel,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET verification_level = ?2, verification_source = 'user' WHERE id = ?1",
        params![task, level],
    )?;
    Ok(())
}

/// Sets or clears "the level was raised and the missing validation hasn't been reported yet".
pub fn set_validation_pending(
    conn: &Connection,
    task: TaskId,
    pending: bool,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET validation_pending = ?2 WHERE id = ?1",
        params![task, pending],
    )?;
    Ok(())
}

/// Records when the independent review passed.
pub fn set_review_passed(conn: &Connection, task: TaskId, at: Timestamp) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET review_passed_at = ?2 WHERE id = ?1",
        params![task, to_millis(at)],
    )?;
    Ok(())
}

/// Takes a passed review back: the builder reported a head the pass wasn't given on.
pub fn clear_review_passed(conn: &Connection, task: TaskId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET review_passed_at = NULL WHERE id = ?1",
        params![task],
    )?;
    Ok(())
}

/// Sets the number of recorded review rounds (the first review plus each re-review).
pub fn set_review_rounds(conn: &Connection, task: TaskId, rounds: u8) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET review_rounds = ?2 WHERE id = ?1",
        params![task, i64::from(rounds)],
    )?;
    Ok(())
}

/// An open review task of `builder` other than `except`. "Open" excludes closed tasks and those on
/// a side exit.
pub fn open_review_of(
    conn: &Connection,
    builder: TaskId,
    except: Option<TaskId>,
) -> Result<Option<TaskId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM tasks
             WHERE builds_on = ?1 AND kind = 'review'
               AND status NOT IN ('closed', 'failed', 'stopped', 'discarded')
               AND (?2 IS NULL OR id <> ?2)
             ORDER BY id DESC LIMIT 1",
            params![builder, except],
            |r| r.get(0),
        )
        .optional()?)
}

/// The newest review task of `builder`, with its status and crewmate.
pub fn last_reviewer(
    conn: &Connection,
    builder: TaskId,
) -> Result<Option<(TaskId, TaskStatus, Option<AgentId>)>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT t.id, t.status,
                    (SELECT a.id FROM agents AS a
                     WHERE a.task_id = t.id AND a.role = 'crewmate' ORDER BY a.id DESC LIMIT 1)
             FROM tasks AS t
             WHERE t.builds_on = ?1 AND t.kind = 'review'
             ORDER BY t.id DESC LIMIT 1",
            params![builder],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// Every clear review report on `builder`'s review tasks, oldest first.
pub fn review_reports(conn: &Connection, builder: TaskId) -> Result<Vec<ReportRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT r.task_id FROM reports AS r JOIN tasks AS t ON t.id = r.task_id
         WHERE t.builds_on = ?1 AND t.kind = 'review' AND r.source = 'report_call'
           AND r.review_verdict IS NOT NULL
         GROUP BY r.task_id ORDER BY MIN(r.id)",
    )?;
    let tasks = stmt
        .query_map(params![builder], |r| r.get::<_, TaskId>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for task in tasks {
        out.extend(
            crate::reports::for_task(conn, task)?
                .into_iter()
                .filter(|r| r.review_verdict.is_some()),
        );
    }
    out.sort_by_key(|r| r.id);
    Ok(out)
}

/// The pending held actions of `task` of the given kinds, by wire name (a level change
/// supersedes pending `merge` and `push` actions).
pub fn pending_held_of(
    conn: &Connection,
    task: TaskId,
    kinds: &[&str],
) -> Result<Vec<HeldActionId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, kind FROM held_actions WHERE task_id = ?1 AND status = 'pending' ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![task], |r| {
            Ok((r.get::<_, HeldActionId>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, kind)| kinds.contains(&kind.as_str()))
        .map(|(id, _)| id)
        .collect())
}

/// What the context block's task line needs from verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineFacts {
    pub status: TaskStatus,
    pub facts: VerificationFacts,
    /// The open review task reviewing this one.
    pub reviewer: Option<TaskId>,
    /// A decision card is pending for this task.
    pub decision_pending: bool,
    /// The pending decision's reason, as stored (`repair_limit`, `review_round_limit`, …).
    pub hold_reason: Option<String>,
}

/// The facts behind the task line, or `None` if the task doesn't exist.
pub fn line_facts(conn: &Connection, task: TaskId) -> Result<Option<LineFacts>, StoreError> {
    let Some(status) = conn
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            params![task],
            |r| r.get::<_, TaskStatus>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let Some(facts) = facts(conn, task)? else {
        return Ok(None);
    };
    let reviewer = open_review_of(conn, task, None)?;
    let hold = conn
        .query_row(
            "SELECT json_extract(payload, '$.reason') FROM held_actions
             WHERE task_id = ?1 AND kind = 'decision' AND status = 'pending'
             ORDER BY id DESC LIMIT 1",
            params![task],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(Some(LineFacts {
        status,
        facts,
        reviewer,
        decision_pending: hold.is_some(),
        hold_reason: hold.flatten(),
    }))
}

/// The context block's verification part of a task's line, or `None` when verification has nothing
/// to say: "committed locally · in review by T-0046", "held · 3 repair attempts used · decision is
/// with you", "held · review still has findings after 2 re-reviews · decision is with you", "held ·
/// the review is blocked · decision is with you", "stuck · decision is with you", "review passed ·
/// pushing".
pub fn task_line(conn: &Connection, task: TaskId) -> Result<Option<String>, StoreError> {
    let Some(f) = line_facts(conn, task)? else {
        return Ok(None);
    };
    Ok(line_for(&f))
}

/// [`task_line`] from its facts (pure).
pub fn line_for(f: &LineFacts) -> Option<String> {
    const WITH_YOU: &str = "decision is with you";
    match f.status {
        TaskStatus::InReview => Some(match f.reviewer {
            Some(r) => format!("committed locally · in review by {r}"),
            None => "committed locally · waiting for its review".to_owned(),
        }),
        TaskStatus::Held => {
            let why = match f.hold_reason.as_deref() {
                Some("repair_limit") => {
                    format!("{} repair attempts used", f.facts.repair.attempts)
                }
                Some("review_round_limit") => {
                    "review still has findings after 2 re-reviews".to_owned()
                }
                Some("needs_user") => "the review is blocked".to_owned(),
                _ if f.facts.repair.held => {
                    format!("{} repair attempts used", f.facts.repair.attempts)
                }
                _ => return Some(format!("held · {WITH_YOU}")),
            };
            Some(format!("held · {why} · {WITH_YOU}"))
        }
        TaskStatus::Stuck if f.decision_pending => Some(format!("stuck · {WITH_YOU}")),
        TaskStatus::Running if f.facts.review_passed_at.is_some() => {
            Some("review passed · pushing".to_owned())
        }
        _ => None,
    }
}
