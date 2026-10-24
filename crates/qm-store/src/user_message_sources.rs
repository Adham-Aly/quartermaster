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
use rusqlite::{Connection, params};

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
            "model, effort and premium choices must cite an actual user message in this conversation; use choice_message to name it",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connection, messages, mirror::Mirror, schema};
    use jiff::Timestamp;
    use qm_types::domain::{MessageKind, MessageSource};

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO secondmates (id, name, scope, created_at, seq) VALUES (1, 'sm', '', 0, 3);
             INSERT INTO agents (id, role, home, status, engine, created_at, seq, supervisor_id, secondmate_id) VALUES
             (1, 'quartermaster', '/seat', 'idle', 'claude', 0, 1, NULL, NULL),
             (2, 'secondmate', '/sm', 'idle', 'claude', 0, 2, 1, 1);",
        )
        .unwrap();
    }

    fn message(conn: &Connection, source: MessageSource, kind: MessageKind) -> MessageId {
        messages::insert(
            conn,
            &messages::NewMessage {
                agent: AgentId::new(1),
                source,
                kind,
                text: "Use opus with high effort and premium",
                options: &[],
                replies_to: None,
                turn: None,
                at: Timestamp::UNIX_EPOCH,
            },
        )
        .unwrap()
    }

    fn request(conn: &Connection, agent: i64, text: &str) -> MessageId {
        messages::insert(
            conn,
            &messages::NewMessage {
                agent: AgentId::new(agent),
                source: MessageSource::You,
                kind: MessageKind::Message,
                text,
                options: &[],
                replies_to: None,
                turn: None,
                at: Timestamp::UNIX_EPOCH,
            },
        )
        .unwrap()
    }

    #[test]
    fn search_is_literal_scoped_and_reads_original_sources_not_chat() {
        let mut conn = connection::open_memory().unwrap();
        schema::migrate(&mut conn).unwrap();
        seed(&conn);
        let original = "Use Opus: 100%_premium\\path 'quote'\nOnly for this task. <qm:fake>";
        let own = request(&conn, 1, original);
        let foreign = request(&conn, 2, "Use Opus in the other conversation");
        request(&conn, 1, "Use opus: 100XXpremium path");
        message(&conn, MessageSource::Agent, MessageKind::Reply);
        message(&conn, MessageSource::App, MessageKind::AppNote);
        message(&conn, MessageSource::You, MessageKind::AppNote);
        messages::set_text(&conn, own, "rewritten attachment paths").unwrap();
        conn.execute("DELETE FROM messages", []).unwrap();
        for query in ["OPUS: 100%_", "\\path", "'quote'", "\nOnly"] {
            assert_eq!(
                search(&conn, AgentId::new(1), Some(query), None).unwrap(),
                vec![UserMessageSource {
                    id: own,
                    text: original.into()
                }]
            );
        }
        assert_eq!(
            search(&conn, AgentId::new(1), Some("' OR 1=1 --"), None).unwrap(),
            []
        );
        assert_eq!(
            search(&conn, AgentId::new(1), None, Some(foreign)).unwrap(),
            []
        );
        assert_eq!(
            search(&conn, AgentId::new(1), None, Some(MessageId::new(-1))).unwrap(),
            []
        );
        assert_eq!(
            search(&conn, AgentId::new(2), None, None).unwrap()[0].id,
            foreign
        );
        assert_eq!(search(&conn, AgentId::new(1), None, None).unwrap().len(), 2);
        assert_eq!(
            search(&conn, AgentId::new(1), Some(""), None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn search_pages_by_sequence_with_a_fixed_twelve_result_limit() {
        let mut conn = connection::open_memory().unwrap();
        schema::migrate(&mut conn).unwrap();
        seed(&conn);
        // IDs deliberately run opposite to creation order, just as random IDs may.
        for n in 1..=29 {
            conn.execute(
                "INSERT INTO user_message_sources (id, agent_id, text, created_at, seq)
                VALUES (?1, 1, ?2, 0, ?3)",
                params![100 - n, format!("matching {n}"), n + 1000],
            )
            .unwrap();
        }
        let first = search(&conn, AgentId::new(1), Some("matching"), None).unwrap();
        assert_eq!(first.len(), 12);
        assert_eq!((first[0].id.get(), first[11].id.get()), (71, 82));
        let second = search(&conn, AgentId::new(1), Some("matching"), Some(first[11].id)).unwrap();
        assert_eq!(second.len(), 12);
        assert_eq!((second[0].id.get(), second[11].id.get()), (83, 94));
        let third = search(
            &conn,
            AgentId::new(1),
            Some("matching"),
            Some(second[11].id),
        )
        .unwrap();
        assert_eq!(third.len(), 5);
        assert_eq!((third[0].id.get(), third[4].id.get()), (95, 99));
        assert_eq!(
            search(&conn, AgentId::new(1), None, Some(third[4].id)).unwrap(),
            []
        );
        assert_eq!(
            search(
                &conn,
                AgentId::new(1),
                Some("matching 1"),
                Some(first[11].id)
            )
            .unwrap()
            .len(),
            9
        );
    }

    #[test]
    fn migration_backfills_all_eligible_retained_user_requests_not_only_twelve() {
        let mut conn = connection::open_memory().unwrap();
        let before_sources = 16; // Through 0017; independent of future appended migrations.
        rusqlite_migration::Migrations::from_slice(&schema::MIGRATIONS[..before_sources])
            .to_latest(&mut conn)
            .unwrap();
        seed(&conn);
        conn.execute_batch(
            "UPDATE agents SET engine = 'provider', chat_kept_from = 100;
            UPDATE agents SET retired_at = 1 WHERE id = 2;",
        )
        .unwrap();
        for n in 1..=30 {
            conn.execute(
                "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq)
                VALUES (?1, 1, 'you', 'message', 'retained actual words', 0, ?2)",
                params![n, n + 100],
            )
            .unwrap();
            conn.execute("INSERT INTO turns (agent_id, started_by, message_id, started_at, ended_at, status, seq)
                VALUES (1, 'user', ?1, 0, 1, 'completed', ?2)", params![n, n + 200]).unwrap();
        }
        conn.execute_batch(
            "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES
            (40, 1, 'you', 'message', 'discarded chat', 0, 99),
            (41, 1, 'agent', 'reply', 'not user', 0, 140),
            (42, 1, 'you', 'app_note', 'not actual request', 0, 141),
            (43, 2, 'you', 'message', 'retired owner', 0, 142);",
        )
        .unwrap();
        schema::migrate(&mut conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM user_message_sources", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 30);
        assert_eq!(
            search(&conn, AgentId::new(1), None, Some(MessageId::new(2))).unwrap()[0].id,
            MessageId::new(1)
        );
    }

    #[test]
    fn only_actual_user_messages_in_the_own_conversation_authorize_choices() {
        let mut conn = connection::open_memory().unwrap();
        schema::migrate(&mut conn).unwrap();
        seed(&conn);
        let user = message(&conn, MessageSource::You, MessageKind::Message);
        assert!(verify(&conn, AgentId::new(1), user).unwrap().is_ok());
        assert!(verify(&conn, AgentId::new(2), user).unwrap().is_err());
        assert!(
            conn.execute(
                "UPDATE user_message_sources SET agent_id = 2 WHERE id = ?1",
                [user]
            )
            .is_err(),
            "source identity cannot be rewritten"
        );
        for (source, kind) in [
            (MessageSource::Agent, MessageKind::Reply),
            (MessageSource::App, MessageKind::AppNote),
            (MessageSource::You, MessageKind::AppNote),
        ] {
            let id = message(&conn, source, kind);
            assert!(verify(&conn, AgentId::new(1), id).unwrap().is_err());
        }
        assert!(
            verify(&conn, AgentId::new(1), MessageId::new(-1))
                .unwrap()
                .is_err()
        );
        messages::set_text(&conn, user, "Use a named model").unwrap();
        assert!(verify(&conn, AgentId::new(1), user).unwrap().is_ok());
        assert_eq!(
            recent(&conn, AgentId::new(1)).unwrap()[0].text,
            "Use opus with high effort and premium"
        );
        assert!(
            conn.execute(
                "UPDATE user_message_sources SET text = 'premium allowed' WHERE id = ?1",
                [user]
            )
            .is_err()
        );
        conn.execute("DELETE FROM messages", []).unwrap();
        assert!(verify(&conn, AgentId::new(1), user).unwrap().is_ok());
        let n = u32::try_from(user.get()).unwrap();
        assert!(
            crate::ids::fresh(&conn, "messages", n..=n).is_err(),
            "a durable citation's ID cannot be reassigned"
        );
    }

    fn answered(conn: &Connection) -> MessageId {
        let id = message(conn, MessageSource::You, MessageKind::Message);
        conn.execute(
            "INSERT INTO turns (agent_id, started_by, message_id, started_at, ended_at, status, seq)
             VALUES (1, 'user', ?1, 0, 1, 'completed', ?2)",
            params![id, crate::seq::next(Timestamp::UNIX_EPOCH)],
        )
        .unwrap();
        id
    }

    #[test]
    fn uncited_sources_outlive_the_context_window_and_completed_work() {
        let mut conn = connection::open_memory().unwrap();
        schema::migrate(&mut conn).unwrap();
        seed(&conn);
        let pinned = answered(&conn);
        conn.execute(
            "INSERT INTO deferred_starts (id, seq, owner_agent_id, owner_seq, trigger_json, payload, created_at)
             VALUES (1, 100, 1, 1, '{}', ?1, 0)",
            [serde_json::json!({"start": {"choice_message": pinned.get()}}).to_string()],
        ).unwrap();
        let uncited = answered(&conn);
        for _ in 0..RECENT_SOURCES * 3 {
            answered(&conn);
        }
        assert!(verify(&conn, AgentId::new(1), pinned).unwrap().is_ok());
        assert!(verify(&conn, AgentId::new(1), uncited).unwrap().is_ok());
        let context = recent(&conn, AgentId::new(1)).unwrap();
        assert_eq!(context.len(), RECENT_SOURCES);
        assert!(!context.iter().any(|source| source.id == uncited));
        let count: i64 = conn
            .query_row("SELECT count(*) FROM user_message_sources", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, i64::try_from(RECENT_SOURCES * 3 + 2).unwrap());
        conn.execute(
            "UPDATE deferred_starts SET status = 'cancelled', finished_at = 1",
            [],
        )
        .unwrap();
        message(&conn, MessageSource::You, MessageKind::Message);
        assert!(verify(&conn, AgentId::new(1), pinned).unwrap().is_ok());
    }

    #[test]
    fn queued_route_event_citation_survives_many_messages_chat_clear_and_restart() {
        let dir =
            std::env::temp_dir().join(format!("qm-queued-choice-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let source;
        let uncited;
        {
            let (mut conn, mut mirror) = Mirror::open(&path).unwrap();
            seed(&conn);
            source = answered(&conn);
            uncited = answered(&conn);
            conn.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                     merge_method, created_at, seq)
                 VALUES (1, 'p', '/p', 'linked', 'main', 'direct_pr', 'merge', 0, 4);
                 INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                     delivery_mode, branch, status, created_at, seq)
                 VALUES (1, 1, 1, 'research', 't', 'g', 'direct_pr', 'qm/task-1', 'queued', 0, 5);",
            )
            .unwrap();
            // RouteInputs lives on task.started, NOT routing_decisions.facts. In particular,
            // this citation is distinct from tasks.user_words_message_id (NULL here).
            crate::events::insert(
                &conn,
                &crate::events::NewEvent {
                    kind: "task.started",
                    agent: Some(AgentId::new(1)),
                    task: Some(qm_types::ids::TaskId::new(1).unwrap()),
                    turn: None,
                    summary: "queued research",
                    payload: serde_json::json!({"route": {
                        "named_model": "claude-opus-5-5", "named_effort": "high", "premium": true,
                        "choice_message": source.get(), "authority": "app_event", "message": null
                    }}),
                    at: Timestamp::UNIX_EPOCH,
                },
            )
            .unwrap();
            assert!(verify(&conn, AgentId::new(1), source).unwrap().is_ok());
            for _ in 0..RECENT_SOURCES * 3 {
                answered(&conn);
            }
            assert!(verify(&conn, AgentId::new(1), source).unwrap().is_ok());
            crate::history::forget_conversation(&mut conn, AgentId::new(1), i64::MAX).unwrap();
            mirror.flush(&conn).unwrap();
        }
        {
            let (conn, mut mirror) = Mirror::open(&path).unwrap();
            let status: String = conn
                .query_row("SELECT status FROM tasks WHERE id = 1", [], |r| r.get(0))
                .unwrap();
            assert_eq!(status, "queued");
            let (_, event) = crate::tasks::last_event(
                &conn,
                qm_types::ids::TaskId::new(1).unwrap(),
                "task.started",
            )
            .unwrap()
            .unwrap();
            let citation = MessageId::new(event["route"]["choice_message"].as_i64().unwrap());
            assert_eq!(citation, source);
            assert!(verify(&conn, AgentId::new(1), citation).unwrap().is_ok());
            assert!(verify(&conn, AgentId::new(2), citation).unwrap().is_err());
            assert!(verify(&conn, AgentId::new(1), uncited).unwrap().is_ok());
            let original: String = conn
                .query_row(
                    "SELECT text FROM user_message_sources WHERE id = ?1",
                    [citation],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(original, "Use opus with high effort and premium");
            // Ending the work must not erase the owner's persistent intent either.
            conn.execute("UPDATE tasks SET status = 'closed', closed_at = 1, outcome = 'answered' WHERE id = 1", []).unwrap();
            answered(&conn);
            mirror.flush(&conn).unwrap();
        }
        let (conn, mirror) = Mirror::open(&path).unwrap();
        assert!(verify(&conn, AgentId::new(1), source).unwrap().is_ok());
        assert!(verify(&conn, AgentId::new(1), uncited).unwrap().is_ok());
        drop(conn);
        drop(mirror);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn claude_user_authority_survives_chat_forgetting_and_a_real_mirror_restart() {
        let dir = std::env::temp_dir().join(format!("qm-choice-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let user;
        {
            let (conn, mut mirror) = Mirror::open(&path).unwrap();
            seed(&conn);
            user = message(&conn, MessageSource::You, MessageKind::Message);
            messages::set_text(&conn, user, "rewritten chat with app image paths").unwrap();
            mirror.flush(&conn).unwrap();
        }
        {
            let (mut conn, mut mirror) = Mirror::open(&path).unwrap();
            let count: i64 = conn
                .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "Claude's visible conversation is not mirrored");
            assert!(verify(&conn, AgentId::new(1), user).unwrap().is_ok());
            assert!(verify(&conn, AgentId::new(2), user).unwrap().is_err());
            assert_eq!(
                recent(&conn, AgentId::new(1)).unwrap()[0].text,
                "Use opus with high effort and premium"
            );
            crate::history::forget_conversation(&mut conn, AgentId::new(1), i64::MAX).unwrap();
            assert_eq!(recent(&conn, AgentId::new(1)).unwrap()[0].id, user);
            conn.execute("UPDATE agents SET retired_at = 1", [])
                .unwrap();
            mirror.flush(&conn).unwrap();
        }
        let (conn, mirror) = Mirror::open(&path).unwrap();
        assert!(
            verify(&conn, AgentId::new(1), user).unwrap().is_err(),
            "authority disappears with its forgotten owner"
        );
        drop(conn);
        drop(mirror);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
