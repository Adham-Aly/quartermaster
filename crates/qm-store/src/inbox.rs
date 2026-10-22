//! Queries over the wake inbox that `wakes.rs` leaves out.
//!
//! - [`claim_ids`] delivers named wakes to a running turn: a crewmate's `job_exited` wakes are
//!   claimed into the turn that is running when they arrive and injected. The same `UPDATE …
//!   RETURNING` as `wakes::claim_wakes`, so two claimers never take one wake.
//! - [`pending_count`], [`pending_ids`] and [`exists`] read the inbox. A retired agent's wakes stay
//!   pending forever and are never counted or claimed.

use crate::StoreError;
use crate::time::{from_millis, to_millis};
use crate::wakes::ClaimedWake;
use jiff::Timestamp;
use qm_types::domain::WakeKind;
use qm_types::ids::{AgentId, TaskId, TurnId, WakeId};
use rusqlite::{Connection, TransactionBehavior, params, params_from_iter};

/// Delivers the wakes in `ids` that are still pending, and whose target isn't retired, to `turn`.
/// Returns the claimed ones, oldest first.
pub fn claim_ids(
    conn: &mut Connection,
    ids: &[WakeId],
    turn: TurnId,
    at: Timestamp,
) -> Result<Vec<ClaimedWake>, StoreError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (0..ids.len())
        .map(|i| format!("?{}", i + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values: Vec<rusqlite::types::Value> = vec![turn.get().into(), to_millis(at).into()];
    values.extend(ids.iter().map(|id| rusqlite::types::Value::from(id.get())));

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claimed = {
        let mut stmt = tx.prepare(&format!(
            "UPDATE wakes SET delivered_turn_id = ?1, delivered_at = ?2
             WHERE id IN ({placeholders})
               AND delivered_at IS NULL AND consumed_at IS NULL
               AND target_agent_id IN (SELECT id FROM agents WHERE retired_at IS NULL)
             RETURNING id, kind, payload, dedupe_key, task_id, created_at, seq"
        ))?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |r| {
            Ok((
                r.get::<_, WakeId>(0)?,
                r.get::<_, WakeKind>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<TaskId>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        // `RETURNING` gives no order: the claim is read back oldest first.
        let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort_by_key(|row| row.6);
        for (id, kind, payload, dedupe_key, task, created, _) in rows {
            out.push(ClaimedWake {
                id,
                kind,
                payload: serde_json::from_str(&payload).map_err(|source| StoreError::Json {
                    column: "wakes.payload",
                    source,
                })?,
                dedupe_key,
                task,
                created_at: from_millis("wakes.created_at", created)?,
            });
        }
        out
    };
    tx.commit()?;
    Ok(claimed)
}

/// How many of `agent`'s wakes are neither delivered nor consumed; zero for a retired agent.
pub fn pending_count(conn: &Connection, agent: AgentId) -> Result<u32, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM wakes AS w
         JOIN agents AS a ON a.id = w.target_agent_id
         WHERE w.target_agent_id = ?1 AND w.delivered_at IS NULL AND w.consumed_at IS NULL
           AND a.retired_at IS NULL",
        params![agent],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The pending wakes of `agent` of any of `kinds` (every kind when `kinds` is empty), oldest
/// first; none for a retired agent.
pub fn pending_ids(
    conn: &Connection,
    agent: AgentId,
    kinds: &[WakeKind],
) -> Result<Vec<WakeId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT w.id, w.kind FROM wakes AS w
         JOIN agents AS a ON a.id = w.target_agent_id
         WHERE w.target_agent_id = ?1 AND w.delivered_at IS NULL AND w.consumed_at IS NULL
           AND a.retired_at IS NULL
         ORDER BY w.seq",
    )?;
    let rows = stmt
        .query_map(params![agent], |r| {
            Ok((r.get::<_, WakeId>(0)?, r.get::<_, WakeKind>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, kind)| kinds.is_empty() || kinds.contains(kind))
        .map(|(id, _)| id)
        .collect())
}

/// Whether a wake with this dedupe key was ever written for `agent`, consumed or not.
pub fn exists(conn: &Connection, agent: AgentId, dedupe_key: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM wakes WHERE target_agent_id = ?1 AND dedupe_key = ?2)",
        params![agent, dedupe_key],
        |r| r.get(0),
    )?)
}
