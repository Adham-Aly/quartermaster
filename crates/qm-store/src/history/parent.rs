//! Deliver the minimal facts before deleting their task/request dependencies.
//!
//! The durable wake is self-contained (no task or request FK). The source marker stays in RAM
//! for the run, including after consumption: a pump batch read before clear must still dedupe.
//! Only markers for unconsumed wakes need disk retention; no old batch survives a restart.

use crate::parent_channel::{self, FactEvent};
use crate::wakes::{self, NewWake};
use crate::{StoreError, seq};
use qm_types::domain::WakeKind;
use rusqlite::{OptionalExtension, Transaction, params};

const KINDS: &[&str] = &[
    "task.started",
    "task.reported",
    "task.status",
    "task.closed",
    "pr.opened",
    "checks.passed",
    "checks.failed",
    "pr.merged",
];

// Deliberately do not copy summaries, reports, titles, or arbitrary payload fields.
fn outcome(event: &FactEvent) -> Option<serde_json::Value> {
    let p = &event.payload;
    let kind = match event.kind.as_str() {
        "task.started" => "started",
        "task.reported" => "reported",
        "task.closed" => "closed",
        "task.status" => match p.get("to")?.as_str()? {
            "held" => "held",
            "stuck" => "stuck",
            "landed" => "landed",
            "failed" => "failed",
            _ => return None,
        },
        "pr.opened" => "pr_ready",
        "checks.passed" => "ci_passed",
        "checks.failed" => "ci_failed",
        "pr.merged" => "merged",
        _ => return None,
    };
    let mut fact = serde_json::json!({
        "type": kind, "task": event.task, "kind": event.task_kind,
        "at": event.at.to_string(),
    });
    if let Some(pr) = p.get("pr").and_then(serde_json::Value::as_i64) {
        fact["pr"] = pr.into();
    }
    if let Some(value) = p.get("outcome").and_then(serde_json::Value::as_str)
        && matches!(
            value,
            "done"
                | "blocked"
                | "needs_decision"
                | "landed"
                | "answered"
                | "failed"
                | "stopped"
                | "discarded"
        )
    {
        fact["outcome"] = value.into();
    }
    if let Some(sha) = p
        .get("merge_commit")
        .or_else(|| p.get("head"))
        .and_then(serde_json::Value::as_str)
        && sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        fact["sha"] = sha.chars().take(7).collect::<String>().into();
    }
    Some(fact)
}

fn mark(tx: &Transaction<'_>, event: i64, wake: Option<i64>) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO forgotten_parent_facts (id, wake_id, seq) VALUES (?1, ?2, ?3) \
         ON CONFLICT (id) DO NOTHING",
        params![event, wake, seq::next_millis(0)],
    )?;
    Ok(())
}

pub(super) fn deliver_and_forget(tx: &Transaction<'_>) -> Result<(), StoreError> {
    // A correlation is request-wide, not task ownership. While any sibling is retained,
    // only facts attributed by their source event or fact-event row link may be removed;
    // judgments and the sibling's facts (including pending deliveries) still belong to it.
    tx.execute_batch(
        "CREATE TEMP TABLE forgetting_requests AS
         SELECT DISTINCT secondmate_request_id AS id FROM tasks
         WHERE id IN (SELECT id FROM temp.forgetting_tasks) AND secondmate_request_id IS NOT NULL;
         CREATE TEMP TABLE forgetting_parent_rows AS
         SELECT p.id FROM parent_channel p
         WHERE correlation_id IN (SELECT id FROM temp.forgetting_requests)
         AND (NOT EXISTS (SELECT 1 FROM tasks t WHERE t.secondmate_request_id = p.correlation_id
                          AND t.id NOT IN (SELECT id FROM temp.forgetting_tasks))
              OR (p.judgment IS NULL AND
                  (json_extract(p.facts, '$.source_event') IN
                       (SELECT id FROM events WHERE task_id IN (SELECT id FROM temp.forgetting_tasks))
                   OR p.id IN (SELECT json_extract(payload, '$.row') FROM events
                               WHERE kind = 'secondmate.facts'
                               AND task_id IN (SELECT id FROM temp.forgetting_tasks)))));",
    )?;
    // Already-pumped facts may still be waiting in the inbox. Preserve those deliveries, not
    // judgment wakes (which quote the old request and report), and detach the task reference.
    let sources: Vec<(i64, Option<i64>)> = tx
        .prepare(
            "SELECT json_extract(p.facts, '$.source_event'), w.id FROM parent_channel p
         LEFT JOIN wakes w ON w.dedupe_key IN (printf('parent:R-%04d:%d', p.correlation_id, p.id),
                                              printf('parent:C-%04d:%d', p.correlation_id, p.id))
         WHERE p.id IN (SELECT id FROM temp.forgetting_parent_rows)
         AND json_extract(p.facts, '$.source_event') IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (source, wake) in sources {
        mark(tx, source, wake)?;
    }
    deliver_unpumped(tx)?;
    forget_parent_rows(tx)
}

fn deliver_unpumped(tx: &Transaction<'_>) -> Result<(), StoreError> {
    let after = parent_channel::cursor(tx)?.unwrap_or(0);
    let scan = parent_channel::events_after(tx, after, KINDS, usize::MAX)?;
    for event in scan.rows {
        let forgetting: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM temp.forgetting_tasks WHERE id = ?1)",
            [event.task],
            |r| r.get(0),
        )?;
        if !forgetting || parent_channel::has_source_event(tx, event.id)? {
            continue;
        }
        let Some(fact) = outcome(&event) else {
            continue;
        };
        if event.kind == "task.started" {
            tx.execute(
                "UPDATE secondmate_requests SET status = 'open' WHERE id = ?1 AND status = 'answered'",
                [event.correlation],
            )?;
        }
        let parent: Option<qm_types::ids::AgentId> = tx
            .query_row(
                "SELECT supervisor_id FROM agents WHERE secondmate_id = ?1 AND role = 'secondmate'
             ORDER BY seq DESC LIMIT 1",
                [event.secondmate],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        // Use the task supervisor as a fallback only for legacy routing without an SM agent.
        let target = match parent {
            Some(parent) => parent,
            None => tx.query_row(
                "SELECT supervisor_id FROM tasks WHERE id = ?1",
                [event.task],
                |r| r.get(0),
            )?,
        };
        let text = format!("{} · {}: {fact}", event.secondmate, event.correlation);
        let wake = wakes::insert_wake(
            tx,
            &NewWake {
                target,
                kind: WakeKind::ParentReport,
                payload: serde_json::json!({"text": text, "short": format!("{} fact", event.task),
                "correlation": event.correlation, "fact": fact}),
                dedupe_key: format!("forgotten-parent:{}", event.id),
                task: None,
                at: event.at,
            },
        )?;
        mark(tx, event.id.get(), wake.map(qm_types::ids::WakeId::get))?;
    }
    Ok(())
}

fn forget_parent_rows(tx: &Transaction<'_>) -> Result<(), StoreError> {
    // Correlation IDs read `R-`; rows written before that read `C-`.
    tx.execute_batch(
        "CREATE TEMP TABLE correlation_prefixes(prefix TEXT NOT NULL);
         INSERT INTO correlation_prefixes VALUES ('R-'), ('C-');
         UPDATE wakes SET task_id = NULL WHERE id IN
             (SELECT wake_id FROM forgotten_parent_facts WHERE wake_id IS NOT NULL);
         DELETE FROM wakes WHERE
             (dedupe_key IN (SELECT printf('parent:' || prefix || '%04d:%d', correlation_id, id)
                             FROM parent_channel, temp.correlation_prefixes
                             WHERE id IN (SELECT id FROM temp.forgetting_parent_rows))
              OR (json_extract(payload, '$.correlation') IN
                      (SELECT printf(prefix || '%04d', id) FROM temp.forgetting_requests, temp.correlation_prefixes)
                  AND json_extract(payload, '$.correlation') NOT IN
                      (SELECT printf(prefix || '%04d', secondmate_request_id) FROM tasks, temp.correlation_prefixes
                       WHERE secondmate_request_id IS NOT NULL
                       AND id NOT IN (SELECT id FROM temp.forgetting_tasks))))
             AND id NOT IN (SELECT wake_id FROM forgotten_parent_facts WHERE wake_id IS NOT NULL);
         DELETE FROM events WHERE kind IN ('secondmate.facts', 'secondmate.report')
             AND json_extract(payload, '$.row') IN
             (SELECT id FROM temp.forgetting_parent_rows);
         DELETE FROM parent_channel WHERE id IN (SELECT id FROM temp.forgetting_parent_rows);
         DELETE FROM events WHERE kind LIKE 'secondmate.%'
             AND json_extract(payload, '$.correlation') IN
                 (SELECT printf(prefix || '%04d', id) FROM temp.forgetting_requests, temp.correlation_prefixes)
             AND json_extract(payload, '$.correlation') NOT IN
                 (SELECT printf(prefix || '%04d', secondmate_request_id) FROM tasks, temp.correlation_prefixes WHERE secondmate_request_id IS NOT NULL
                  AND id NOT IN (SELECT id FROM temp.forgetting_tasks));
         DELETE FROM secondmate_requests WHERE id IN (SELECT id FROM temp.forgetting_requests)
             AND id NOT IN (SELECT secondmate_request_id FROM tasks WHERE secondmate_request_id IS NOT NULL
                            AND id NOT IN (SELECT id FROM temp.forgetting_tasks));
         DROP TABLE temp.forgetting_parent_rows;
         DROP TABLE temp.forgetting_requests;
         DROP TABLE temp.correlation_prefixes;",
    )?;
    Ok(())
}
