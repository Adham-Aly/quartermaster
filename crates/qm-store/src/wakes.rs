//! The durable wake inbox.
//!
//! - A wake is written with [`insert_wake`] **in the same transaction as the fact that caused it**,
//!   with a dedupe key (`checks:T-0042:9c0d2e4:failed`), so the same news never wakes anyone twice.
//! - A turn takes its wakes with [`claim_wakes`]: `BEGIN IMMEDIATE`, then one `UPDATE … RETURNING`
//!   that marks pending wakes delivered to that turn, so two claimers can never take the same wake,
//!   even on separate connections.
//! - Delivery is at-least-once: [`consume_wakes`] marks a turn's wakes consumed only when the turn
//!   finishes. If the app stops mid-turn, startup marks the turn interrupted and
//!   [`release_unconsumed`] puts its wakes back in the inbox.

use crate::StoreError;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::WakeKind;
use qm_types::ids::{AgentId, TaskId, TurnId, WakeId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// A wake to write.
#[derive(Clone, Debug, PartialEq)]
pub struct NewWake {
    pub target: AgentId,
    pub kind: WakeKind,
    /// What the wake carries, rendered into the target's next turn.
    pub payload: serde_json::Value,
    /// Unique per target: the same key never wakes the same agent twice.
    pub dedupe_key: String,
    /// The task it is about, if any.
    pub task: Option<TaskId>,
    pub at: Timestamp,
}

/// A wake delivered to a turn.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedWake {
    pub id: WakeId,
    pub kind: WakeKind,
    pub payload: serde_json::Value,
    pub dedupe_key: String,
    pub task: Option<TaskId>,
    pub created_at: Timestamp,
}

/// Writes a wake unless one with the same dedupe key already exists for that target. Returns its
/// ID, or `None` if it was a duplicate. Call it inside the transaction that records the fact.
pub fn insert_wake(conn: &Connection, wake: &NewWake) -> Result<Option<WakeId>, StoreError> {
    let payload = serde_json::to_string(&wake.payload).map_err(|source| StoreError::Json {
        column: "wakes.payload",
        source,
    })?;
    let id = conn
        .query_row(
            "INSERT INTO wakes (target_agent_id, kind, payload, dedupe_key, task_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (target_agent_id, dedupe_key) DO NOTHING
             RETURNING id",
            params![
                wake.target,
                wake.kind,
                payload,
                wake.dedupe_key,
                wake.task,
                to_millis(wake.at)
            ],
            |r| r.get(0),
        )
        .optional()?;
    Ok(id)
}

/// The kinds of the target's pending wakes (not yet delivered), each once.
pub fn pending_kinds(conn: &Connection, target: AgentId) -> Result<Vec<WakeKind>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT kind FROM wakes
         WHERE target_agent_id = ?1 AND delivered_at IS NULL AND consumed_at IS NULL
         ORDER BY kind",
    )?;
    let kinds = stmt
        .query_map(params![target], |r| r.get::<_, WakeKind>(0))?
        .collect::<Result<_, _>>()?;
    Ok(kinds)
}

/// Delivers up to `limit` of the target's pending wakes to `turn`, oldest first.
pub fn claim_wakes(
    conn: &mut Connection,
    target: AgentId,
    turn: TurnId,
    at: Timestamp,
    limit: u32,
) -> Result<Vec<ClaimedWake>, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut claimed = {
        let mut stmt = tx.prepare(
            "UPDATE wakes SET delivered_turn_id = ?1, delivered_at = ?2
             WHERE id IN (
                 SELECT id FROM wakes
                 WHERE target_agent_id = ?3 AND delivered_at IS NULL AND consumed_at IS NULL
                 ORDER BY id
                 LIMIT ?4
             )
             RETURNING id, kind, payload, dedupe_key, task_id, created_at",
        )?;
        let rows = stmt.query_map(params![turn, to_millis(at), target, limit], |r| {
            Ok((
                r.get::<_, WakeId>(0)?,
                r.get::<_, WakeKind>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<TaskId>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, kind, payload, dedupe_key, task, created) = row?;
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
    claimed.sort_by_key(|w| w.id);
    Ok(claimed)
}

/// Marks every wake delivered to `turn` consumed: call it when the turn finishes. Returns how
/// many.
pub fn consume_wakes(conn: &Connection, turn: TurnId, at: Timestamp) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE wakes SET consumed_at = ?2 WHERE delivered_turn_id = ?1 AND consumed_at IS NULL",
        params![turn, to_millis(at)],
    )?)
}

/// Puts the unconsumed wakes delivered to `turn` back in the inbox (the turn was interrupted).
/// Returns how many.
pub fn release_unconsumed(conn: &Connection, turn: TurnId) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "UPDATE wakes SET delivered_turn_id = NULL, delivered_at = NULL
         WHERE delivered_turn_id = ?1 AND consumed_at IS NULL",
        params![turn],
    )?)
}
