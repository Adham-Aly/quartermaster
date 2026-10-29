//! Durable, app-recorded provenance for user-authorized model choices.
//!
//! Only `messages::insert` records a source, and only actual `you`/`message` rows qualify.
//! Tools cannot manufacture sources by supplying text, a boolean, or a turn ID. A citation is
//! scoped to its conversation; the core separately checks any delegation through a routed request.
//!
//! Original sources are persistent user intent: all remain until their owner is forgotten, even
//! if uncited or the work they authorized ends. Clearing/compacting chat is not revoking authority.
//! `kept::KEPT` and owner retirement handle their lifetime; the recent context window only limits
//! presentation. This also keeps queued `task.started` `payload.route` inputs valid after a restart,
//! without depending on a citation in `routing_decisions.facts` (which has no such field).

use crate::StoreError;
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, MessageId};
use rusqlite::{Connection, OptionalExtension, params};

/// Requests shown in the recent context window, not a retention limit. Older original sources
/// remain valid citations; merely seeing a request does not authorize any particular field.
pub const RECENT_SOURCES: usize = 12;

/// An immutable original user request (not a rewritten chat line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserMessageSource {
    pub id: MessageId,
    pub text: String,
}

/// The latest requests in this conversation, oldest first, without truncation.
pub fn recent(conn: &Connection, agent: AgentId) -> Result<Vec<UserMessageSource>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, text FROM (SELECT id, text, seq FROM user_message_sources
         WHERE agent_id = ?1 ORDER BY seq DESC LIMIT ?2) ORDER BY seq",
    )?;
    Ok(stmt
        .query_map(
            params![agent, i64::try_from(RECENT_SOURCES).unwrap_or(i64::MAX)],
            |r| {
                Ok(UserMessageSource {
                    id: r.get(0)?,
                    text: r.get(1)?,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?)
}

/// Searches full original words in this conversation, latest first, at most twelve.
/// `query` is a case-insensitive literal substring (SQLite's built-in case folding).
/// The cursor is resolved to creation order within this conversation, never compared as an ID.
/// Missing or foreign cursors yield no rows, without revealing another conversation's source.
pub fn search(
    conn: &Connection,
    agent: AgentId,
    query: Option<&str>,
    before: Option<MessageId>,
) -> Result<Vec<UserMessageSource>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, text FROM user_message_sources
         WHERE agent_id = ?1
           AND (?2 IS NULL OR instr(lower(text), lower(?2)) > 0)
           AND (?3 IS NULL OR seq < (
               SELECT seq FROM user_message_sources WHERE id = ?3 AND agent_id = ?1
           ))
         ORDER BY seq DESC LIMIT 12",
    )?;
    Ok(stmt
        .query_map(params![agent, query, before], |r| {
            Ok(UserMessageSource {
                id: r.get(0)?,
                text: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Verifies app-recorded user authorship and conversation ownership, not semantic intent.
pub fn verify(
    conn: &Connection,
    agent: AgentId,
    message: MessageId,
) -> Result<Result<(), Refusal>, StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM user_message_sources WHERE id = ?1 AND agent_id = ?2)",
        params![message, agent],
        |r| r.get(0),
    )?;
    Ok(if exists {
        Ok(())
    } else {
        Err(Refusal::new(
            "model, tier, effort and premium choices must cite an actual user message in this conversation; use choice_message to name it",
        ))
    })
}

/// The words a brief or routed request copies from `message`: its chat text (which carries any
/// attachment paths the app added), else its kept original once chat has dropped it. Refused unless
/// `message` is an actual user message in `agent`'s own conversation.
pub fn words_of(
    conn: &Connection,
    agent: AgentId,
    message: MessageId,
) -> Result<Result<String, Refusal>, StoreError> {
    let original: Option<String> = conn
        .query_row(
            "SELECT text FROM user_message_sources WHERE id = ?1 AND agent_id = ?2",
            params![message, agent],
            |r| r.get(0),
        )
        .optional()?;
    let Some(original) = original else {
        return Ok(Err(Refusal::new(format!(
            "words_message {} isn't one of the user's own messages in this conversation; use an id from <qm:user_message_sources> or find_user_requests",
            message.get()
        ))));
    };
    Ok(Ok(crate::messages::text(conn, message)?.unwrap_or(original)))
}
