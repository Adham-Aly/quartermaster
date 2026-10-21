//! Your standing instructions: lasting preferences the Quartermaster sees in its context block
//! every turn until dropped. Each names the message and the turn it came from; dropping keeps the
//! row and records the turn that dropped it.

use crate::StoreError;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::ids::{MessageId, StandingInstructionId, TurnId};
use rusqlite::{Connection, OptionalExtension, params};

/// A stored standing instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionRow {
    pub id: StandingInstructionId,
    pub text: String,
    pub message: MessageId,
    pub turn: TurnId,
    pub created_at: Timestamp,
    pub dropped_at: Option<Timestamp>,
}

/// Keeps a standing instruction from your message on that turn.
pub fn save(
    conn: &Connection,
    text: &str,
    message: MessageId,
    turn: TurnId,
    at: Timestamp,
) -> Result<StandingInstructionId, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO standing_instructions (text, message_id, turn_id, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5)
         RETURNING id",
        params![text, message, turn, to_millis(at), seq::next(at)],
        |r| r.get(0),
    )?)
}

/// One standing instruction, dropped or not.
pub fn get(
    conn: &Connection,
    id: StandingInstructionId,
) -> Result<Option<InstructionRow>, StoreError> {
    let raw = conn
        .query_row(
            "SELECT id, text, message_id, turn_id, created_at, dropped_at
             FROM standing_instructions WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, StandingInstructionId>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, MessageId>(2)?,
                    r.get::<_, TurnId>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(id, text, message, turn, created, dropped)| {
        Ok(InstructionRow {
            id,
            text,
            message,
            turn,
            created_at: from_millis("standing_instructions.created_at", created)?,
            dropped_at: dropped
                .map(|ms| from_millis("standing_instructions.dropped_at", ms))
                .transpose()?,
        })
    })
    .transpose()
}

/// Drops a standing instruction on that turn. Returns `false` when it doesn't exist or was
/// already dropped (nothing changes then).
pub fn drop(
    conn: &Connection,
    id: StandingInstructionId,
    turn: TurnId,
    at: Timestamp,
) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE standing_instructions SET dropped_at = ?2, dropped_turn_id = ?3
         WHERE id = ?1 AND dropped_at IS NULL",
        params![id, to_millis(at), turn],
    )?;
    Ok(changed == 1)
}

/// Every standing instruction not dropped, oldest first.
pub fn active(conn: &Connection) -> Result<Vec<(StandingInstructionId, String)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, text FROM standing_instructions WHERE dropped_at IS NULL ORDER BY seq",
    )?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
