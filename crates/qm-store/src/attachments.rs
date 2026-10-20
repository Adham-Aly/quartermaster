//! What the attachment cleanup needs to know: whether a pasted image's folder is still in use.
//!
//! The folders themselves live on disk (`attachments/message-<id>/`); their message rows say when
//! they were sent, and their paths travel in plain text. A folder is in use while open work was
//! given its path: an open task's goal, your words, instructions, constraints or later steps, a
//! message to its crewmate, or an open request to a secondmate.

use crate::StoreError;
use crate::time::from_millis;
use jiff::Timestamp;
use qm_types::ids::MessageId;
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
