//! The conversation: one `messages` row per chat line.
//!
//! | Shown | Stored as |
//! |---|---|
//! | Your message | `source = you`, `kind = message`, the text neutralised before it gets here |
//! | A reply or a question | `source = agent`, `kind = reply` / `question`, `options` |
//! | A tick beside your message | `kind = acknowledgement`, `replies_to` your message (`source = agent` for the Quartermaster's, `app` for the app's) |
//! | A line from the app | `source = app`, `kind = app_note` |
//!
//! Acknowledgements are never lines of their own: the newest one replying to a message is shown
//! beside it ([`latest_acknowledgements`]), so a later one replaces an earlier one.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{MessageKind, MessageSource};
use qm_types::ids::space;
use qm_types::ids::{AgentId, MessageId, TurnId};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::collections::BTreeMap;

/// A chat line to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage<'a> {
    /// Whose conversation it belongs to.
    pub agent: AgentId,
    pub source: MessageSource,
    pub kind: MessageKind,
    pub text: &'a str,
    /// A question's one-click answers; stored as JSON only when there are some.
    pub options: &'a [String],
    pub replies_to: Option<MessageId>,
    pub turn: Option<TurnId>,
    pub at: Timestamp,
}

/// A stored chat line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRow {
    pub id: MessageId,
    pub agent: AgentId,
    pub source: MessageSource,
    pub kind: MessageKind,
    pub text: String,
    pub options: Vec<String>,
    pub replies_to: Option<MessageId>,
    pub turn: Option<TurnId>,
    pub at: Timestamp,
}

/// Stores one chat line.
pub fn insert(conn: &Connection, message: &NewMessage<'_>) -> Result<MessageId, StoreError> {
    let options = if message.options.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(message.options).map_err(|source| StoreError::Json {
                column: "messages.options",
                source,
            })?,
        )
    };
    Ok(conn.query_row(
        "INSERT INTO messages (agent_id, source, kind, text, options, replies_to, turn_id, created_at, seq,
                                id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         RETURNING id",
        params![
            message.agent,
            message.source,
            message.kind,
            message.text,
            options,
            message.replies_to,
            message.turn,
            to_millis(message.at),
            seq::next(message.at),
            ids::fresh(conn, "messages", space::MESSAGE)?,
        ],
        |r| r.get(0),
    )?)
}

const COLUMNS: &str = "id, agent_id, source, kind, text, options, replies_to, turn_id, created_at";

type Raw = (
    MessageId,
    AgentId,
    MessageSource,
    MessageKind,
    String,
    Option<String>,
    Option<MessageId>,
    Option<TurnId>,
    i64,
);

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
    ))
}

fn parse(
    (id, agent, source, kind, text, options, replies_to, turn, at): Raw,
) -> Result<MessageRow, StoreError> {
    let options = match options {
        Some(json) => serde_json::from_str(&json).map_err(|source| StoreError::Json {
            column: "messages.options",
            source,
        })?,
        None => Vec::new(),
    };
    Ok(MessageRow {
        id,
        agent,
        source,
        kind,
        text,
        options,
        replies_to,
        turn,
        at: from_millis("messages.created_at", at)?,
    })
}

fn collect(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<MessageRow>, StoreError> {
    stmt.query_map(params, raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(parse)
        .collect()
}

/// The newest `limit` lines of an agent's conversation, oldest first. Acknowledgements are left
/// out: they are shown beside the message they reply to, never as lines.
pub fn load_recent(
    conn: &Connection,
    agent: AgentId,
    limit: usize,
) -> Result<Vec<MessageRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM (
             SELECT {COLUMNS}, seq FROM messages
             WHERE agent_id = ?1 AND kind <> 'acknowledgement'
             ORDER BY seq DESC LIMIT ?2
         ) ORDER BY seq"
    ))?;
    collect(
        &mut stmt,
        params![agent, i64::try_from(limit).unwrap_or(i64::MAX)],
    )
}

/// Whether your message is still owed a turn: one for the message itself, and one more for each
/// time you chose Ask again on its notice, less the turns it has had. A request to run it that
/// arrives twice (recovery at startup found it waiting, and its own signal came too) finds it
/// answered the second time.
pub fn owed_turn(conn: &Connection, message: MessageId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM turns WHERE message_id = ?1)
              < 1 + (SELECT count(*) FROM events
                      WHERE kind = ?2
                        AND json_extract(payload, '$.message') = ?1
                        AND json_extract(payload, '$.answer') = ?3)",
        params![
            message,
            crate::events::NOTICE_ANSWERED,
            crate::events::NoticeChoice::AskAgain.as_str()
        ],
        |r| r.get(0),
    )?)
}

/// Your messages to `agent` that no turn has taken yet, oldest first: they were stored but the
/// app stopped before their turn started.
pub fn waiting(conn: &Connection, agent: AgentId) -> Result<Vec<MessageId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT m.id FROM messages AS m
         WHERE m.agent_id = ?1 AND m.source = 'you' AND m.kind = 'message'
           AND NOT EXISTS (SELECT 1 FROM turns AS t WHERE t.message_id = m.id)
         ORDER BY m.seq",
    )?;
    Ok(stmt
        .query_map(params![agent], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The questions still waiting on you from `agents`, oldest first: an agent's question with no
/// later message from you to that agent. Any message you send closes it, since that is what the
/// asking agent's next turn reads anyway.
pub fn open_questions(
    conn: &Connection,
    agents: &[AgentId],
) -> Result<Vec<MessageRow>, StoreError> {
    if agents.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=agents.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let columns = COLUMNS
        .split(", ")
        .map(|c| format!("q.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT {columns} FROM messages AS q
         WHERE q.kind = 'question' AND q.source = 'agent' AND q.agent_id IN ({placeholders})
           AND NOT EXISTS (SELECT 1 FROM messages AS y
                           WHERE y.agent_id = q.agent_id AND y.source = 'you' AND y.seq > q.seq)
         ORDER BY q.seq"
    ))?;
    collect(&mut stmt, params_from_iter(agents.iter()))
}

/// One message.
pub fn get(conn: &Connection, message: MessageId) -> Result<Option<MessageRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM messages WHERE id = ?1"),
            params![message],
            raw,
        )
        .optional()?;
    raw.map(parse).transpose()
}

/// Replaces a message's text, in the transaction that stored it: your message gets its pasted
/// images' paths once its ID (their folder's name) is known.
pub fn set_text(conn: &Connection, message: MessageId, text: &str) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE messages SET text = ?2 WHERE id = ?1",
        params![message, text],
    )?;
    Ok(())
}

/// A message's text, as stored.
pub fn text(conn: &Connection, message: MessageId) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT text FROM messages WHERE id = ?1",
            params![message],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether `agent` already showed a `kind` line with this text (ignoring surrounding whitespace)
/// on `turn`: the same reply or question sent twice in one turn is shown once.
pub fn shown_on_turn(
    conn: &Connection,
    agent: AgentId,
    turn: TurnId,
    kind: MessageKind,
    text: &str,
) -> Result<bool, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT text FROM messages
          WHERE agent_id = ?1 AND turn_id = ?2 AND source = 'agent' AND kind = ?3",
    )?;
    let texts = stmt.query_map(params![agent, turn, kind], |r| r.get::<_, String>(0))?;
    for shown in texts {
        if shown?.trim() == text.trim() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The newest acknowledgement replying to each of `messages` that has one.
pub fn latest_acknowledgements(
    conn: &Connection,
    messages: &[MessageId],
) -> Result<BTreeMap<MessageId, String>, StoreError> {
    let mut out = BTreeMap::new();
    if messages.is_empty() {
        return Ok(out);
    }
    let placeholders = (1..=messages.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT replies_to, text FROM messages
         WHERE kind = 'acknowledgement' AND replies_to IN ({placeholders})
         ORDER BY seq"
    ))?;
    let rows = stmt
        .query_map(params_from_iter(messages.iter()), |r| {
            Ok((r.get::<_, MessageId>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (message, text) in rows {
        // Oldest first, so the newest one wins.
        out.insert(message, text);
    }
    Ok(out)
}

/// What the chat header says about the turn a line came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOrigin {
    /// Your message started the turn.
    pub by_you: bool,
    /// For a wake turn: the first claimed wake's `payload.short` ("T-0043 reported").
    pub short: Option<String>,
}

/// Who started each of `turns`, and for wake turns the first wake's short line.
pub fn turn_origins(
    conn: &Connection,
    turns: &[TurnId],
) -> Result<BTreeMap<TurnId, TurnOrigin>, StoreError> {
    let mut out = BTreeMap::new();
    if turns.is_empty() {
        return Ok(out);
    }
    let placeholders = (1..=turns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT t.id, t.started_by,
                (SELECT json_extract(w.payload, '$.short') FROM wakes AS w
                 WHERE w.delivered_turn_id = t.id ORDER BY w.seq LIMIT 1)
         FROM turns AS t WHERE t.id IN ({placeholders})"
    ))?;
    let rows = stmt
        .query_map(params_from_iter(turns.iter()), |r| {
            Ok((
                r.get::<_, TurnId>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<rusqlite::types::Value>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (turn, started_by, short) in rows {
        let short = match short {
            Some(rusqlite::types::Value::Text(s)) if !s.trim().is_empty() => Some(s),
            _ => None,
        };
        out.insert(
            turn,
            TurnOrigin {
                by_you: started_by == "user",
                short,
            },
        );
    }
    Ok(out)
}

/// The `seq` of the last turn before the agent's latest `/clear` (`seat.cleared`), or 0: only
/// turns after it count for a new session's summary.
const CLEARED_AFTER: &str = "(SELECT COALESCE(MAX(json_extract(payload, '$.after_turn')), 0)
       FROM events WHERE kind = 'seat.cleared' AND agent_id = ?1)";

/// The visible lines of an agent's last `turns` turns since your last `/clear`, oldest first, for
/// the summary a new session opens with: your messages that started them, and the replies,
/// questions and acknowledgements written during them. App notes are left out.
pub fn for_summary(
    conn: &Connection,
    agent: AgentId,
    turns: usize,
) -> Result<Vec<MessageRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "WITH recent AS (
             SELECT id, message_id FROM turns WHERE agent_id = ?1 AND seq > {CLEARED_AFTER}
             ORDER BY seq DESC LIMIT ?2
         )
         SELECT {COLUMNS} FROM messages
         WHERE agent_id = ?1 AND kind <> 'app_note'
           AND (turn_id IN (SELECT id FROM recent)
                OR id IN (SELECT message_id FROM recent WHERE message_id IS NOT NULL))
         ORDER BY seq"
    ))?;
    collect(
        &mut stmt,
        params![agent, i64::try_from(turns).unwrap_or(i64::MAX)],
    )
}

/// Whether the agent has had any turn since your last `/clear` (or ever, if you never ran it).
pub fn has_turns(conn: &Connection, agent: AgentId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE agent_id = ?1 AND seq > {CLEARED_AFTER})"
        ),
        params![agent],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use qm_types::ids::AgentId;

    #[test]
    fn the_newest_lines_query_runs_on_a_fresh_store() {
        // Startup reads every agent's conversation, so the query must run before any line exists.
        let mut conn = Connection::open_in_memory().expect("memory database");
        schema::migrate(&mut conn).expect("the schema");
        let rows = load_recent(&conn, AgentId::new(1), 5).expect("the query runs");
        assert_eq!(rows.len(), 0);
    }

    fn line(
        conn: &Connection,
        agent: i64,
        source: MessageSource,
        kind: MessageKind,
        ms: i64,
    ) -> MessageId {
        insert(
            conn,
            &NewMessage {
                agent: AgentId::new(agent),
                source,
                kind,
                text: "text",
                options: &[],
                replies_to: None,
                turn: None,
                at: Timestamp::from_millisecond(ms).expect("a time"),
            },
        )
        .expect("the line is stored")
    }

    #[test]
    fn a_later_message_from_you_closes_a_question() {
        let mut conn = Connection::open_in_memory().expect("memory database");
        schema::migrate(&mut conn).expect("the schema");
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .expect("no agents rows needed");
        let asked = line(&conn, 1, MessageSource::Agent, MessageKind::Question, 1_000);
        let other = line(&conn, 2, MessageSource::Agent, MessageKind::Question, 2_000);
        line(&conn, 3, MessageSource::Agent, MessageKind::Question, 2_500);
        let open = |conn: &Connection| -> Vec<MessageId> {
            open_questions(conn, &[AgentId::new(1), AgentId::new(2)])
                .expect("the query runs")
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(open(&conn), vec![asked, other]);
        // An answer to another agent, or a reply from the agent itself, leaves it open.
        line(&conn, 2, MessageSource::You, MessageKind::Message, 3_000);
        line(&conn, 1, MessageSource::Agent, MessageKind::Reply, 3_500);
        assert_eq!(open(&conn), vec![asked]);
        line(&conn, 1, MessageSource::You, MessageKind::Message, 4_000);
        assert_eq!(open(&conn).len(), 0);
        assert_eq!(open_questions(&conn, &[]).expect("the query runs").len(), 0);
    }
}
