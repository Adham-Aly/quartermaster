//! Forgetting: what the working database lets go of (plan 3.4, Q2, Q8).
//!
//! A conversation is forgotten when it is cleared or compacted ([`forget_conversation`]), and
//! closed tasks beyond the newest [`KEEP_CLOSED_TASKS`] are forgotten ([`forget_closed_tasks`]).
//! Forgetting is deletion in dependency order under deferred foreign keys, so the rows that name a
//! deleted row are either deleted too or have the reference nulled first. The nulling runs with the
//! append-only guards lifted (`mirror::with_guards_lifted`). A forgotten task number is reserved for
//! the rest of the run (`ids::reserve`), so a compaction summary's T-numbers aren't handed out again
//! before a restart. The file drops them at the next flush.

use crate::kept;
use crate::mirror::with_guards_lifted;
use crate::{StoreError, ids};
use qm_types::ids::{AgentId, MessageId, TurnId};
use rusqlite::{Connection, Transaction, params};

/// How many closed tasks stay in memory. Older ones are forgotten (Q8).
pub const KEEP_CLOSED_TASKS: usize = 200;

#[cfg(test)]
mod explicit_tests;
mod parent;

/// Forgets `agent`'s conversation before the turn with sequence `before` (`i64::MAX` for all of it):
/// its turns, messages and session lines. Returns the number of turns forgotten.
pub fn forget_conversation(
    conn: &mut Connection,
    agent: AgentId,
    before: i64,
) -> Result<usize, StoreError> {
    forget_conversation_except(conn, agent, before, &[])
}

/// Forgets the captured conversation while retaining queued inputs, even when they precede
/// summarized output in sequence order. Rows at or after `before` also stay.
pub fn forget_conversation_except(
    conn: &mut Connection,
    agent: AgentId,
    before: i64,
    protected: &[MessageId],
) -> Result<usize, StoreError> {
    const TURNS: &str = "SELECT id FROM turns WHERE agent_id = ?1 AND seq < ?2";
    // A user's message precedes the turn it starts. Keep inputs of every retained turn, even
    // when the input's own sequence is below the compaction cutoff.
    const MESSAGES: &str = "SELECT id FROM messages WHERE agent_id = ?1 AND seq < ?2 \
        AND id NOT IN (SELECT id FROM temp.protected_messages) \
        AND id NOT IN (SELECT message_id FROM turns WHERE message_id IS NOT NULL \
                       AND NOT (agent_id = ?1 AND seq < ?2))";
    with_guards_lifted(conn, |tx| {
        tx.execute_batch(
            "PRAGMA defer_foreign_keys = ON;
            CREATE TEMP TABLE protected_messages (id INTEGER PRIMARY KEY);",
        )?;
        for message in protected {
            tx.execute(
                "INSERT OR IGNORE INTO temp.protected_messages VALUES (?1)",
                [message],
            )?;
        }
        let p = params![agent, before];
        tx.execute(
            &format!("UPDATE events SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
            p,
        )?;
        tx.execute(
            &format!(
                "UPDATE schedules SET created_turn_id = NULL WHERE created_turn_id IN ({TURNS})"
            ),
            p,
        )?;
        tx.execute(
            &format!("UPDATE secondmate_requests SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
            p,
        )?;
        tx.execute(
            &format!("UPDATE standing_instructions SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
            p,
        )?;
        tx.execute(
            &format!("UPDATE standing_instructions SET message_id = NULL WHERE message_id IN ({MESSAGES})"),
            p,
        )?;
        tx.execute(
            &format!("UPDATE tasks SET user_words_message_id = NULL WHERE user_words_message_id IN ({MESSAGES})"),
            p,
        )?;
        tx.execute(
            &format!("UPDATE messages SET replies_to = NULL WHERE replies_to IN ({MESSAGES})"),
            p,
        )?;
        tx.execute(
            &format!("UPDATE messages SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
            p,
        )?;
        tx.execute(
            &format!(
                "DELETE FROM session_lines WHERE (agent_id = ?1 AND seq < ?2 \
                     AND (message_id IS NULL OR message_id NOT IN (SELECT id FROM temp.protected_messages))) \
                 OR turn_id IN ({TURNS}) OR message_id IN ({MESSAGES})"
            ),
            p,
        )?;
        let turns = tx.execute(&format!("DELETE FROM turns WHERE id IN ({TURNS})"), p)?;
        tx.execute(&format!("DELETE FROM messages WHERE id IN ({MESSAGES})"), p)?;
        forget_seen_events(tx)?;
        tx.execute_batch("DROP TABLE temp.protected_messages;")?;
        Ok(turns)
    })
}

/// Forgets `agent`'s conversation before `turn`, the turn a compaction happened in: that turn and
/// later ones stay. Returns the number of turns forgotten.
pub fn forget_before_turn(
    conn: &mut Connection,
    agent: AgentId,
    turn: TurnId,
) -> Result<usize, StoreError> {
    let before: i64 =
        conn.query_row("SELECT seq FROM turns WHERE id = ?1", [turn], |r| r.get(0))?;
    forget_conversation(conn, agent, before)
}

/// Forgets every closed task beyond the newest `keep`, with what hangs off it: reports, links,
/// jobs, processes, routing decisions, wakes and held actions, and the crewmates that worked on it
/// with their conversations. Tasks with unread parent-channel facts stay even beyond the cap,
/// until the pump advances its durable cursor. Returns the forgotten task numbers.
pub fn forget_closed_tasks(conn: &mut Connection, keep: usize) -> Result<Vec<u32>, StoreError> {
    forget_closed(conn, keep, false)
}

/// Explicit clear/compaction: discards every closed task's details, even with unread facts.
/// Minimal pending parent outcomes are delivered to durable, task-independent wakes first.
/// Unlike cap eviction, this never waits for the parent pump or block watermarks.
pub fn forget_closed_task_details(conn: &mut Connection) -> Result<Vec<u32>, StoreError> {
    forget_closed(conn, 0, true)
}

fn forget_closed(
    conn: &mut Connection,
    keep: usize,
    explicit: bool,
) -> Result<Vec<u32>, StoreError> {
    let keep = i64::try_from(keep).unwrap_or(i64::MAX);
    let forgotten = with_guards_lifted(conn, |tx| {
        tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        let ids: Vec<i64> = tx
            .prepare(
                "SELECT id FROM (SELECT id FROM tasks WHERE status = 'closed' \
                 ORDER BY seq DESC LIMIT -1 OFFSET ?1) \
                 WHERE (?2 OR id NOT IN (SELECT task_id FROM events WHERE task_id IS NOT NULL \
                                  AND seq > (SELECT COALESCE(MAX(event_seq), 0) \
                                             FROM parent_channel_cursor)))",
            )?
            .query_map(params![keep, explicit], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        tx.execute_batch(
            "CREATE TEMP TABLE forgetting_tasks (id INTEGER PRIMARY KEY);
             CREATE TEMP TABLE forgetting_agents (id INTEGER PRIMARY KEY);",
        )?;
        for id in &ids {
            tx.execute("INSERT INTO temp.forgetting_tasks (id) VALUES (?1)", [id])?;
        }
        tx.execute(
            "INSERT INTO temp.forgetting_agents (id) \
             SELECT id FROM agents WHERE task_id IN (SELECT id FROM temp.forgetting_tasks)",
            [],
        )?;
        // Even cap eviction must not leave request/report history orphaned in RAM.
        parent::deliver_and_forget(tx)?;
        // Delete detail events while their ownership is still known; nulling first would make
        // cap-evicted details impossible for a later explicit clear to identify.
        tx.execute_batch(
            "UPDATE events SET
                     summary = CASE WHEN task_id IS NULL THEN kind
                                    ELSE printf('T-%04d %s', task_id, substr(kind, 9)) END,
                     payload = json_object('supervisor', COALESCE(
                         json_extract(payload, '$.supervisor'),
                         (SELECT supervisor_id FROM tasks WHERE id = events.task_id)))
                 WHERE kind LIKE 'outcome.%' AND
                     (task_id IN (SELECT id FROM temp.forgetting_tasks)
                      OR agent_id IN (SELECT id FROM temp.forgetting_agents)
                      OR turn_id IN (SELECT id FROM turns WHERE agent_id IN
                                     (SELECT id FROM temp.forgetting_agents)));
                 DELETE FROM events WHERE kind NOT LIKE 'outcome.%' AND
                     (task_id IN (SELECT id FROM temp.forgetting_tasks)
                      OR agent_id IN (SELECT id FROM temp.forgetting_agents)
                      OR turn_id IN (SELECT id FROM turns WHERE agent_id IN
                                     (SELECT id FROM temp.forgetting_agents)));",
        )?;
        forget_tasks(tx)?;
        forget_seen_events(tx)?;
        tx.execute_batch("DROP TABLE temp.forgetting_agents; DROP TABLE temp.forgetting_tasks;")?;
        Ok(ids)
    })?;
    let numbers: Vec<u32> = forgotten
        .iter()
        .filter_map(|id| u32::try_from(*id).ok())
        .collect();
    ids::reserve("tasks", numbers.iter().copied());
    Ok(numbers)
}

/// Deletes the tasks in `temp.forgetting_tasks`, their dependants and the crewmates in
/// `temp.forgetting_agents`, nulling what open rows reference.
fn forget_tasks(tx: &Transaction<'_>) -> Result<(), StoreError> {
    const TASKS: &str = "SELECT id FROM temp.forgetting_tasks";
    const AGENTS: &str = "SELECT id FROM temp.forgetting_agents";
    const TURNS: &str =
        "SELECT id FROM turns WHERE agent_id IN (SELECT id FROM temp.forgetting_agents)";
    const MESSAGES: &str =
        "SELECT id FROM messages WHERE agent_id IN (SELECT id FROM temp.forgetting_agents)";
    for column in ["builds_on", "parent_task_id", "retry_of"] {
        tx.execute(
            &format!("UPDATE tasks SET {column} = NULL WHERE {column} IN ({TASKS})"),
            [],
        )?;
    }
    tx.execute(
        &format!("UPDATE events SET task_id = NULL WHERE task_id IN ({TASKS})"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE events SET agent_id = NULL WHERE agent_id IN ({AGENTS})"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE events SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE schedules SET created_turn_id = NULL WHERE created_turn_id IN ({TURNS})"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE secondmate_requests SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE standing_instructions SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
        [],
    )?;
    tx.execute(
        &format!(
            "UPDATE standing_instructions SET message_id = NULL WHERE message_id IN ({MESSAGES})"
        ),
        [],
    )?;
    tx.execute(
        &format!("UPDATE tasks SET user_words_message_id = NULL WHERE user_words_message_id IN ({MESSAGES})"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE messages SET turn_id = NULL WHERE turn_id IN ({TURNS})"),
        [],
    )?;
    for statement in [
        format!("DELETE FROM reports WHERE task_id IN ({TASKS})"),
        format!("DELETE FROM task_links WHERE blocker IN ({TASKS}) OR blocked IN ({TASKS})"),
        format!("DELETE FROM jobs WHERE task_id IN ({TASKS}) OR agent_id IN ({AGENTS})"),
        format!("DELETE FROM processes WHERE task_id IN ({TASKS}) OR agent_id IN ({AGENTS})"),
        format!(
            "DELETE FROM routing_decisions WHERE task_id IN ({TASKS}) OR agent_id IN ({AGENTS})"
        ),
        format!("DELETE FROM wakes WHERE task_id IN ({TASKS}) OR target_agent_id IN ({AGENTS})"),
        // Card anchors have no task/agent FK: their ownership is the held action itself.
        format!(
            "DELETE FROM events WHERE kind = 'held.shown' AND json_extract(payload, '$.held') IN \
             (SELECT id FROM held_actions WHERE task_id IN ({TASKS}) OR asked_by IN ({AGENTS}))"
        ),
        format!("DELETE FROM held_actions WHERE task_id IN ({TASKS}) OR asked_by IN ({AGENTS})"),
        format!(
            "DELETE FROM session_lines WHERE agent_id IN ({AGENTS}) OR task_id IN ({TASKS}) \
             OR turn_id IN ({TURNS}) OR message_id IN ({MESSAGES})"
        ),
        format!("DELETE FROM block_watermarks WHERE agent_id IN ({AGENTS})"),
        format!("DELETE FROM turns WHERE agent_id IN ({AGENTS})"),
        format!("DELETE FROM messages WHERE agent_id IN ({AGENTS})"),
        format!("DELETE FROM agents WHERE id IN ({AGENTS})"),
        format!("DELETE FROM tasks WHERE id IN ({TASKS})"),
    ] {
        tx.execute(&statement, [])?;
    }
    Ok(())
}

/// Deletes the events every live orchestrator has seen, unless the file keeps them anyway (the
/// block's outcomes, an open task's facts, the app's standing facts).
fn forget_seen_events(tx: &Transaction<'_>) -> Result<(), StoreError> {
    tx.execute(
        &format!(
            "DELETE FROM events WHERE seq <= (SELECT COALESCE(MIN(event_seq), 0) FROM block_watermarks) \
             AND NOT COALESCE(({}), 0)",
            kept::predicate("events")
        ),
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connection, schema};

    pub(super) fn working() -> Connection {
        let mut conn = connection::open_memory().expect("memory database");
        schema::migrate(&mut conn).expect("the schema");
        conn.execute(
            "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                 merge_method, created_at, seq) \
                 VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1)",
            [],
        )
        .expect("a project");
        conn.execute(
            "INSERT INTO agents (id, role, home, status, created_at, seq) \
             VALUES (1, 'quartermaster', '/home', 'idle', 1, 1)",
            [],
        )
        .expect("the quartermaster");
        conn
    }

    pub(super) fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("a count")
    }

    #[test]
    fn forgetting_a_conversation_drops_what_came_before_the_cutoff() {
        let mut conn = working();
        conn.execute_batch(
            "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) \
                 VALUES (1, 1, 'you', 'message', 'old', 1, 1), (2, 1, 'you', 'message', 'older', 2, 3);
             INSERT INTO turns (id, agent_id, started_by, message_id, started_at, ended_at, status, seq) \
                 VALUES (1, 1, 'user', 1, 1, 2, 'completed', 2);
             INSERT INTO turns (id, agent_id, started_by, started_at, status, seq) \
                 VALUES (2, 1, 'app_event', 3, 'running', 4);
             INSERT INTO events (at, kind, summary, payload, turn_id, seq) \
                 VALUES (1, 'you.notice', 'kept', '{}', 1, 5);
             INSERT INTO session_lines (agent_id, turn_id, kind, text, created_at, seq) \
                 VALUES (1, 1, 'agent', 'said', 1, 6), (1, 2, 'agent', 'still going', 3, 7);",
        )
        .expect("a conversation");

        let forgotten = forget_conversation(&mut conn, AgentId::new(1), 4).expect("forget");

        assert_eq!(forgotten, 1, "the completed turn is forgotten");
        assert_eq!(count(&conn, "turns"), 1, "the running turn stays");
        assert_eq!(count(&conn, "messages"), 0, "messages before the cutoff go");
        assert_eq!(
            count(&conn, "session_lines"),
            1,
            "the running turn's line stays"
        );
        let turn: Option<i64> = conn
            .query_row("SELECT turn_id FROM events", [], |r| r.get(0))
            .expect("the event");
        assert_eq!(
            turn, None,
            "an event that named a forgotten turn keeps no reference"
        );
    }

    #[test]
    fn captured_compaction_boundary_removes_output_after_a_protected_queued_input() {
        let mut conn = working();
        conn.execute_batch(
            "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES
                 (1, 1, 'you', 'message', 'old input', 1, 10),
                 (2, 1, 'you', 'message', 'queued input', 1, 30),
                 (3, 1, 'agent', 'reply', 'old reply', 1, 40);
             INSERT INTO turns (id, agent_id, started_by, message_id, started_at, ended_at, status, seq)
                 VALUES (1, 1, 'user', 1, 1, 2, 'completed', 20);
             INSERT INTO session_lines (agent_id, message_id, kind, text, created_at, seq) VALUES
                 (1, 1, 'you', 'old input', 1, 11),
                 (1, 2, 'you', 'queued input', 1, 31),
                 (1, 3, 'agent', 'old reply', 1, 41);",
        ).expect("an input queued before the preceding turn's final reply");
        assert_eq!(
            forget_conversation_except(&mut conn, AgentId::new(1), 50, &[MessageId::new(2)])
                .expect("forget captured conversation"),
            1
        );
        assert_eq!(count(&conn, "turns"), 0);
        assert_eq!(count(&conn, "messages"), 1);
        assert_eq!(count(&conn, "messages WHERE id = 2 AND seq = 30"), 1);
        assert_eq!(count(&conn, "session_lines"), 1);
        assert_eq!(count(&conn, "session_lines WHERE message_id = 2"), 1);
        assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
        // The temporary protection set is per call, not sticky across later clear/compaction.
        forget_conversation(&mut conn, AgentId::new(1), i64::MAX).expect("clear");
        assert_eq!(count(&conn, "messages"), 0);
        assert_eq!(count(&conn, "session_lines"), 0);
    }

    #[test]
    fn a_compaction_forgets_the_turns_before_its_own_turn() {
        let mut conn = working();
        conn.execute_batch(
            "INSERT INTO turns (id, agent_id, started_by, started_at, status, ended_at, seq) \
                 VALUES (1, 1, 'app_event', 1, 'completed', 2, 1);
             INSERT INTO turns (id, agent_id, started_by, started_at, status, seq) \
                 VALUES (2, 1, 'app_event', 3, 'running', 2);
             INSERT INTO session_lines (agent_id, turn_id, kind, text, created_at, seq) \
                 VALUES (1, 1, 'agent', 'before', 1, 3), (1, 2, 'agent', 'during', 3, 4);",
        )
        .expect("a conversation");

        let forgotten =
            forget_before_turn(&mut conn, AgentId::new(1), TurnId::new(2)).expect("forget");

        assert_eq!(forgotten, 1, "the turn before the compacting one goes");
        assert_eq!(count(&conn, "turns"), 1, "the compacting turn stays");
        assert_eq!(count(&conn, "session_lines"), 1, "its line stays");
    }

    #[test]
    fn user_started_compaction_keeps_its_input_but_forgets_prior_history() {
        let mut conn = working();
        conn.execute_batch(
            "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES \
                 (1, 1, 'you', 'message', 'old input', 1, 1), \
                 (2, 1, 'you', 'message', 'current turn input', 1, 100);
             INSERT INTO turns (id, agent_id, started_by, message_id, status, started_at, ended_at, seq) \
                 VALUES (1, 1, 'user', 1, 'completed', 1, 2, 2);
             INSERT INTO turns (id, agent_id, started_by, message_id, status, started_at, seq) \
                 VALUES (3, 1, 'user', 2, 'running', 1, 101);
             INSERT INTO session_lines (agent_id, turn_id, message_id, kind, text, created_at, seq) \
                 VALUES (1, 1, 1, 'you', 'old input', 1, 3), \
                        (1, 3, 2, 'you', 'current turn input', 1, 102);",
        ).expect("user-started conversation");
        assert_eq!(
            forget_before_turn(&mut conn, AgentId::new(1), TurnId::new(3)).expect("compact"),
            1,
        );
        assert_eq!(count(&conn, "messages WHERE id = 2"), 1);
        assert_eq!(count(&conn, "messages WHERE id = 1"), 0);
        assert_eq!(count(&conn, "turns"), 1);
        assert_eq!(count(&conn, "session_lines"), 1);
        assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
        forget_conversation(&mut conn, AgentId::new(1), i64::MAX).expect("clear everything");
        assert_eq!(count(&conn, "messages"), 0);
        assert_eq!(count(&conn, "turns"), 0);
    }

    #[test]
    fn the_cap_keeps_old_tasks_with_unread_facts_until_delivery() {
        let mut conn = working();
        for id in 1..=201 {
            conn.execute(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, \
                 status, outcome, created_at, closed_at, seq) \
                 VALUES (?1, 1, 1, 'build', 't', 'g', 'folder', 'closed', 'answered', 1, 2, ?1)",
                [id],
            ).expect("closed task");
        }
        conn.execute_batch(
            "INSERT INTO parent_channel_cursor (seq, event_seq) VALUES (1, 0);
             INSERT INTO events (id, at, kind, summary, payload, task_id, seq) \
                 VALUES (1, 1, 'task.closed', 'unread fact', '{}', 1, 999);",
        )
        .expect("unread fact");
        assert_eq!(
            forget_closed_tasks(&mut conn, 200).expect("cap"),
            Vec::<u32>::new()
        );
        assert_eq!(count(&conn, "tasks"), 201);
        let task: Option<i64> = conn
            .query_row("SELECT task_id FROM events WHERE id = 1", [], |r| r.get(0))
            .expect("unread fact keeps its task reference");
        assert_eq!(task, Some(1));
        conn.execute("UPDATE parent_channel_cursor SET event_seq = 999", [])
            .expect("delivery");
        assert_eq!(
            forget_closed_tasks(&mut conn, 200).expect("cap after delivery"),
            vec![1]
        );
        assert_eq!(count(&conn, "tasks"), 200);
        assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
    }

    #[test]
    fn closed_tasks_beyond_the_cap_go_fk_clean_and_their_numbers_stay_reserved() {
        let mut conn = working();
        let closed = i64::try_from(KEEP_CLOSED_TASKS).expect("the cap fits in an i64") + 2;
        for id in 1..=closed {
            conn.execute(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, \
                     branch, status, outcome, created_at, closed_at, seq) \
                     VALUES (?1, 1, 1, 'build', 't', 'g', 'direct_pr', ?2, 'closed', 'landed', 1, 1, ?1)",
                params![id, format!("qm/task-{id}")],
            )
            .expect("a closed task");
        }
        conn.execute_batch(
            "INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, \
                 retired_at, seq) VALUES (50, 'crewmate', 1, 1, '/c', 'stopped', 1, 1, 50);
             INSERT INTO turns (id, agent_id, started_by, started_at, ended_at, status, seq) \
                 VALUES (60, 50, 'app_event', 1, 2, 'completed', 60);
             INSERT INTO wakes (id, target_agent_id, kind, payload, dedupe_key, task_id, created_at, seq) \
                 VALUES (1, 1, 'k', '{}', 'd', 1, 1, 1);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, \
                 branch, builds_on, status, created_at, seq) \
                 VALUES (500, 1, 1, 'build', 'open', 'g', 'direct_pr', 'qm/task-500', 1, 'queued', 1, 900);",
        )
        .expect("the crew and an open task that builds on the oldest closed one");

        let mut forgotten = forget_closed_tasks(&mut conn, KEEP_CLOSED_TASKS).expect("forget");
        forgotten.sort_unstable();

        assert_eq!(forgotten, vec![1, 2], "the two oldest closed tasks go");
        assert_eq!(
            count(&conn, "tasks"),
            closed - 2 + 1,
            "the newest 200 and the open task stay"
        );
        let builds_on: Option<i64> = conn
            .query_row("SELECT builds_on FROM tasks WHERE id = 500", [], |r| {
                r.get(0)
            })
            .expect("the open task");
        assert_eq!(
            builds_on, None,
            "an open task that builds on a forgotten one keeps no reference"
        );
        assert_eq!(
            count(&conn, "agents WHERE id = 50"),
            0,
            "the crewmate of a forgotten task goes"
        );
        assert_eq!(
            count(&conn, "turns WHERE id = 60"),
            0,
            "its conversation goes with it"
        );
        assert_eq!(count(&conn, "wakes"), 0, "its wake goes");
        assert!(
            ids::fresh(&conn, "tasks", 1..=2).is_err(),
            "a forgotten task's number is not handed out again in this run"
        );
    }
}
