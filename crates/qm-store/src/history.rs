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
use crate::{StoreError, ids, task_labels};
use qm_types::ids::{AgentId, MessageId, TurnId};
use rusqlite::{Connection, Transaction, params};

/// How many closed tasks stay in memory. Older ones are forgotten (Q8).
pub const KEEP_CLOSED_TASKS: usize = 200;

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
        // The images in the forgotten part of the conversation are no longer in it.
        tx.execute(
            "DELETE FROM attachment_uses WHERE agent_id = ?1 AND seq < ?2 \
                 AND message_id NOT IN (SELECT id FROM temp.protected_messages)",
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
    forget_before_turn_except(conn, agent, turn, &[])
}

/// Forgets history before `turn`, retaining its input and any protected queued inputs even
/// when those inputs were stored before this turn began. Returns the number of turns forgotten.
pub fn forget_before_turn_except(
    conn: &mut Connection,
    agent: AgentId,
    turn: TurnId,
    protected: &[MessageId],
) -> Result<usize, StoreError> {
    let before: i64 =
        conn.query_row("SELECT seq FROM turns WHERE id = ?1", [turn], |r| r.get(0))?;
    forget_conversation_except(conn, agent, before, protected)
}

/// Forgets every closed task beyond the newest `keep`, with what hangs off it: reports, links,
/// jobs, processes, routing decisions, wakes and held actions, and the crewmates that worked on it
/// with their conversations, and the task labels no crewmate names any more. Tasks with unread
/// parent-channel facts stay even beyond the cap, until the pump advances its durable cursor.
/// Tasks with undismissed reports or open dependents stay regardless of the cap.
/// Returns the forgotten crewmate numbers.
pub fn forget_closed_tasks(conn: &mut Connection, keep: usize) -> Result<Vec<u32>, StoreError> {
    forget_closed(conn, keep, false)
}

/// Explicit clear/compaction: discards closed task details except tasks with undismissed reports
/// or open dependents, even with unread facts. Minimal pending parent outcomes are delivered to durable,
/// task-independent wakes first. Unlike cap eviction, this never waits for the parent pump
/// or block watermarks.
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
            .prepare(&format!(
                "SELECT id FROM (SELECT id FROM tasks WHERE status = 'closed' \
                     ORDER BY seq DESC LIMIT -1 OFFSET ?1) \
                     WHERE id NOT IN (SELECT id FROM tasks WHERE {} OR {}) \
                     AND (?2 OR id NOT IN (SELECT task_id FROM events WHERE task_id IS NOT NULL \
                                      AND seq > (SELECT COALESCE(MAX(event_seq), 0) \
                                                 FROM parent_channel_cursor)))",
                kept::PENDING_REPORT,
                kept::NEEDED_BLOCKER
            ))?
            .query_map(params![keep, explicit], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if ids.is_empty() {
            return Ok((Vec::new(), Vec::new()));
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
                                    ELSE printf('C-%04d %s', task_id, substr(kind, 9)) END,
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
        let labels = task_labels::forget_unused(tx)?;
        forget_seen_events(tx)?;
        tx.execute_batch("DROP TABLE temp.forgetting_agents; DROP TABLE temp.forgetting_tasks;")?;
        Ok((ids, labels))
    })?;
    let (forgotten, labels) = forgotten;
    let numbers: Vec<u32> = forgotten
        .iter()
        .filter_map(|id| u32::try_from(*id).ok())
        .collect();
    ids::reserve("tasks", numbers.iter().copied());
    ids::reserve("task_labels", labels);
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
        format!("DELETE FROM user_message_sources WHERE agent_id IN ({AGENTS})"),
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
