//! What the attachment cleanup needs to know: whether a pasted image's folder is still in use.
//!
//! The folders themselves live on disk (`attachments/message-<id>/`); their message rows say when
//! they were sent, and their paths travel in plain text. A folder is in use while open work was
//! given its path: an open task's goal, your words, instructions, constraints or later steps, a
//! message to its crewmate, or an open request to a secondmate; or while an agent that isn't
//! retired still holds it in its current conversation (`attachment_uses`).

use crate::StoreError;
use crate::time::from_millis;
use jiff::Timestamp;
use qm_types::ids::{AgentId, MessageId};
use rusqlite::{Connection, OptionalExtension, params};

/// When message `id` was sent; `None` when there is no such message.
pub fn sent_at(conn: &Connection, id: MessageId) -> Result<Option<Timestamp>, StoreError> {
    let millis: Option<i64> = conn
        .query_row(
            "SELECT created_at FROM messages WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    millis
        .map(|ms| from_millis("messages.created_at", ms))
        .transpose()
}

/// Whether open work was given text containing `needle` (a folder's path).
pub fn in_open_work(conn: &Connection, needle: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tasks t
             WHERE t.status <> 'closed'
               AND (instr(t.goal, ?1) > 0
                    OR instr(t.user_words, ?1) > 0
                    OR instr(t.instructions, ?1) > 0
                    OR instr(t.constraints, ?1) > 0
                    OR instr(t.later_steps, ?1) > 0
                    OR EXISTS (SELECT 1 FROM agents a JOIN messages m ON m.agent_id = a.id
                               WHERE a.task_id = t.id AND instr(m.text, ?1) > 0))
         ) OR EXISTS (
             SELECT 1 FROM secondmate_requests r
             WHERE r.status = 'open'
               AND (instr(r.request, ?1) > 0
                    OR instr(r.user_words, ?1) > 0
                    OR instr(r.instructions, ?1) > 0)
         )",
        params![needle],
        |r| r.get(0),
    )?)
}

/// Records that `agent`'s conversation got the images of message `message`, sent at `at`.
pub fn record_use(
    conn: &Connection,
    message: MessageId,
    agent: AgentId,
    at: Timestamp,
) -> Result<(), StoreError> {
    let millis = crate::time::to_millis(at);
    conn.execute(
        "INSERT OR IGNORE INTO attachment_uses (message_id, agent_id, created_at, seq)
         VALUES (?1, ?2, ?3, ?4)",
        params![message, agent, millis, crate::seq::next(at)],
    )?;
    Ok(())
}

/// Records that the agent message `message` was written to got its images, at the message's time.
pub fn record_use_by_recipient(conn: &Connection, message: MessageId) -> Result<(), StoreError> {
    let row: Option<(AgentId, i64)> = conn
        .query_row(
            "SELECT agent_id, created_at FROM messages WHERE id = ?1",
            params![message],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((agent, millis)) = row {
        record_use(conn, message, agent, from_millis("created_at", millis)?)?;
    }
    Ok(())
}

/// Whether an agent that isn't retired still holds message `message`'s images in its
/// conversation.
pub fn in_live_chat(conn: &Connection, message: MessageId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM attachment_uses u JOIN agents a ON a.id = u.agent_id
                        WHERE u.message_id = ?1 AND a.retired_at IS NULL)",
        params![message],
        |r| r.get(0),
    )?)
}

/// Forgets every use by `agent` (its agent retired, or its conversation was cleared).
pub fn forget_uses(conn: &Connection, agent: AgentId) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM attachment_uses WHERE agent_id = ?1",
        params![agent],
    )?;
    Ok(())
}
