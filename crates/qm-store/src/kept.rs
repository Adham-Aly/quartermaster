//! What `state.db` keeps: the rows of each table that outlive the process (plan section 2).
//!
//! The working database lives in memory (`mirror.rs`), and `state.db` mirrors the rows that
//! satisfy each table's predicate here. Every table is in exactly one of [`KEPT`] or [`VOLATILE`].
//! A predicate may look at other tables, so the order of [`KEPT`] matters
//! when rows are pruned at load: a table's predicate sees the tables before it, already pruned.
//! That is why `agents` comes before `secondmates` (a retired secondmate's row stays while
//! anything still points at its agent, so `null_dangling` never has to null that link) and
//! `block_watermarks`, and `events` comes last.
//!
//! The chat tables ([`CHAT`]) keep only a live in-house agent's current chat: a Claude agent's chat
//! is Claude Code's to keep, and a compaction, a `/clear` or a retirement leaves no copy of what it
//! dropped on disk.
//!
//! The predicates use `COALESCE(…, 0)` in SQL: a NULL result keeps nothing.

/// One mirrored table and the predicate that decides which of its rows are kept.
#[derive(Debug, Clone, Copy)]
pub struct Kept {
    pub table: &'static str,
    /// SQL over the table's own columns, true for the rows to keep.
    pub predicate: &'static str,
}

// `report_read_at` records explicit dismissal, not merely opening a report. A newer report
// reopens the obligation. Keep this rule shared by mirroring and history forgetting.
macro_rules! pending_report {
    () => {
        "kind IN ('lookup', 'research', 'reproduce', 'teach') AND reported_at IS NOT NULL \
         AND (status = 'reported' OR (status = 'closed' AND outcome = 'answered')) \
         AND (report_read_at IS NULL OR report_read_at < reported_at)"
    };
}

pub(crate) const PENDING_REPORT: &str = pending_report!();

// Keep task dependencies under exactly the same rule as the task itself. This is expanded in
// SQL subqueries too, since a live working database still contains delivered closed tasks.
macro_rules! retained_task {
    () => {
        concat!(
            "status <> 'closed' OR (",
            pending_report!(),
            ") OR id IN (SELECT task_id FROM events WHERE task_id IS NOT NULL \
             AND seq > (SELECT COALESCE(MAX(event_seq), 0) FROM parent_channel_cursor))"
        )
    };
}

// An in-house agent's current chat: the rows of a live agent on the provider engine from where its
// kept chat starts (`agents.chat_kept_from`). A Claude agent's chat never reaches the file; neither
// does a retired agent's. The table's own name qualifies `agent_id` and `seq`, since `agents` has a
// `seq` too. The mirror writes these tables row by row (`mirror.rs`), so this predicate must only
// read the row itself and its agent.
macro_rules! current_chat {
    ($table:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM agents AS a WHERE a.id = ",
            $table,
            ".agent_id AND a.retired_at IS NULL AND a.engine = 'provider' AND ",
            $table,
            ".seq >= a.chat_kept_from)"
        )
    };
}

/// The tables whose kept rows follow `current_chat!`: an in-house agent's visible chat. The mirror
/// writes only their changed rows, plus every row of an agent whose chat started or stopped being
/// kept.
pub const CHAT: &[&str] = &["messages", "turns", "session_lines"];

/// The tables `state.db` mirrors, in prune order.
pub const KEPT: &[Kept] = &[
    Kept {
        table: "accounts",
        predicate: "1",
    },
    Kept {
        table: "models",
        predicate: "1",
    },
    Kept {
        table: "placements",
        predicate: "1",
    },
    // Every project, retired ones too: a folder that gets work again comes back with its settings.
    Kept {
        table: "projects",
        predicate: "1",
    },
    // A crewmate is kept only with its task: its row must name one (a CHECK), so a crewmate whose
    // task leaves can't have that reference nulled. Whatever else would keep it (a wake it never
    // took, say) goes with it.
    Kept {
        table: "agents",
        predicate: concat!(
            "(role <> 'crewmate' OR task_id IN (SELECT id FROM tasks WHERE ",
            retained_task!(),
            ")) AND (retired_at IS NULL \
                    OR task_id IN (SELECT id FROM tasks WHERE ",
            retained_task!(),
            ") \
                    OR id IN (SELECT supervisor_id FROM tasks WHERE ",
            retained_task!(),
            ") \
                    OR id IN (SELECT target_agent_id FROM wakes WHERE consumed_at IS NULL) \
                    OR id IN (SELECT agent_id FROM reports \
                              WHERE task_id IN (SELECT id FROM tasks WHERE status <> 'closed')) \
                    OR id IN (SELECT owner_agent_id FROM schedules WHERE status = 'pending') \
                    OR id IN (SELECT owner_agent_id FROM deferred_starts WHERE status = 'pending') \
                    OR id IN (SELECT asked_by FROM held_actions WHERE status = 'pending') \
                    OR id IN (SELECT agent_id FROM routing_decisions \
                              WHERE task_id IN (SELECT id FROM tasks WHERE status <> 'closed')) \
                    OR id IN (SELECT agent_id FROM jobs \
                              WHERE task_id IN (SELECT id FROM tasks WHERE status <> 'closed')))"
        ),
    },
    // User authority outlives chat clearing and engine-owned chat, but not its owner.
    Kept {
        table: "user_message_sources",
        predicate: "agent_id IN (SELECT id FROM agents)",
    },
    // The current chat of each live in-house agent: after `agents`, which they read.
    Kept {
        table: "messages",
        predicate: current_chat!("messages"),
    },
    Kept {
        table: "turns",
        predicate: current_chat!("turns"),
    },
    Kept {
        table: "session_lines",
        predicate: current_chat!("session_lines"),
    },
    // A chat image's use lasts as long as the agent's conversation can: while it isn't retired.
    Kept {
        table: "attachment_uses",
        predicate: "agent_id IN (SELECT id FROM agents WHERE retired_at IS NULL)",
    },
    Kept {
        table: "secondmates",
        predicate: concat!(
            "retired_at IS NULL \
                    OR id IN (SELECT secondmate_id FROM agents WHERE secondmate_id IS NOT NULL) \
                    OR id IN (SELECT secondmate_id FROM secondmate_requests WHERE id IN \
                              (SELECT secondmate_request_id FROM tasks WHERE ",
            retained_task!(),
            "))"
        ),
    },
    Kept {
        table: "secondmate_projects",
        predicate: "secondmate_id IN (SELECT id FROM secondmates WHERE retired_at IS NULL) \
                    AND project_id IN (SELECT id FROM projects \
                    WHERE removed_at IS NULL OR id IN (SELECT project_id FROM tasks WHERE status <> 'closed'))",
    },
    Kept {
        // A closed task stays for undismissed reports as well as unread parent facts.
        // Its request and agents must survive under the same rule.
        table: "tasks",
        predicate: retained_task!(),
    },
    Kept {
        // A task label lasts while a kept crewmate is filed under it, by its label or its folders.
        table: "task_labels",
        predicate: concat!(
            "id IN (SELECT label_id FROM tasks WHERE ",
            retained_task!(),
            ") OR id IN (SELECT folder_label_id FROM tasks WHERE ",
            retained_task!(),
            ")"
        ),
    },
    Kept {
        table: "task_links",
        predicate: "blocker IN (SELECT id FROM tasks WHERE status <> 'closed') \
                    AND blocked IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "reports",
        predicate: concat!(
            "task_id IN (SELECT id FROM tasks WHERE status <> 'closed' OR (",
            pending_report!(),
            "))"
        ),
    },
    Kept {
        table: "held_actions",
        predicate: "status = 'pending' \
                    OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        // A launch still in flight is kept whatever its task: the next start reconciles it
        // against the remote, or goes on watching its release and workflow runs.
        table: "release_launches",
        predicate: "status IN ('proposed', 'pushing', 'unconfirmed') \
                    OR observe_state = 'observing' \
                    OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    // A wake goes with the agent it wakes (after `agents`, which it reads).
    Kept {
        table: "wakes",
        predicate: "target_agent_id IN (SELECT id FROM agents) \
                    AND (consumed_at IS NULL \
                         OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed'))",
    },
    // Terminal rows remain queryable too; a cancelled/started schedule can never replay.
    Kept {
        table: "deferred_starts",
        predicate: "1",
    },
    Kept {
        table: "schedules",
        predicate: "status = 'pending'",
    },
    Kept {
        table: "standing_instructions",
        predicate: "dropped_at IS NULL",
    },
    Kept {
        // Evaluated on the running store too, where closed tasks are still present: so the task
        // side repeats `tasks`' own predicate, and matches the same requests either way.
        table: "secondmate_requests",
        predicate: concat!(
            "(secondmate_id IN (SELECT id FROM secondmates WHERE retired_at IS NULL) \
              AND status = 'open') \
             OR id IN (SELECT secondmate_request_id FROM tasks WHERE ",
            retained_task!(),
            ")"
        ),
    },
    Kept {
        // The requests this table keeps (see `secondmate_requests`): the rows follow their request
        // on disk too, not only at load.
        table: "parent_channel",
        predicate: concat!(
            "correlation_id IN (SELECT id FROM secondmate_requests \
              WHERE (secondmate_id IN (SELECT id FROM secondmates WHERE retired_at IS NULL) \
                     AND status = 'open') \
                 OR id IN (SELECT secondmate_request_id FROM tasks WHERE ",
            retained_task!(),
            "))"
        ),
    },
    Kept {
        table: "jobs",
        predicate: "task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "processes",
        predicate: "ended_at IS NULL",
    },
    Kept {
        table: "routing_decisions",
        predicate: "task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "block_watermarks",
        predicate: "agent_id IN (SELECT id FROM agents)",
    },
    Kept {
        table: "parent_channel_cursor",
        predicate: "1",
    },
    Kept {
        table: "forgotten_parent_facts",
        predicate: "wake_id IN (SELECT id FROM wakes WHERE consumed_at IS NULL)",
    },
    Kept {
        table: "events",
        // Block lines not yet seen by the oldest live orchestrator (from the lower of its watermark
        // and the one before it, 0 for a first turn, so a turn that is rolled back still finds its
        // lines and a reload shows what a first turn showed); the newest
        // five global outcomes plus each secondmate's own five (`payload.supervisor`);
        // everything of an open task; the notices under the messages of a kept chat, your answers
        // on them and the marks that draw a plan reply's note on one line; and the two facts the
        // app keeps for good.
        predicate: concat!(
            "(kind LIKE 'you.%' OR kind LIKE 'task.%' OR kind LIKE 'pr.%' \
                         OR kind LIKE 'checks.%' OR kind LIKE 'secondmate.%' OR kind LIKE 'model.%' \
                         OR kind IN ('model_new', 'model_gone')) \
                        AND seq > (SELECT COALESCE(MIN(floor), 0) FROM \
                                   (SELECT MIN(event_seq, COALESCE(prev_event_seq, 0)) AS floor \
                                    FROM block_watermarks)) \
                        OR seq IN (SELECT seq FROM events WHERE kind LIKE 'outcome.%' \
                                   ORDER BY seq DESC LIMIT 5) \
                        OR seq IN (SELECT seq FROM (SELECT seq, ROW_NUMBER() OVER ( \
                                       PARTITION BY json_extract(payload, '$.supervisor') \
                                       ORDER BY seq DESC) AS rank \
                                   FROM events WHERE kind LIKE 'outcome.%' \
                                   AND json_extract(payload, '$.supervisor') IN \
                                       (SELECT id FROM agents WHERE role = 'secondmate')) WHERE rank <= 5) \
                        OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed') \
                        OR (task_id IN (SELECT id FROM tasks) \
                            AND seq > (SELECT COALESCE(MAX(event_seq), 0) FROM parent_channel_cursor)) \
                        OR (kind IN ('seat.notice', 'you.answered_notice', 'plan.page_reply_note') \
                            AND json_extract(payload, '$.message') IN \
                                (SELECT id FROM messages WHERE ",
            current_chat!("messages"),
            ")) OR kind IN ('starter_tiers_seeded', 'settings.model_listed')"
        ),
    },
];

/// The predicate that keeps rows of `table`.
///
/// # Panics
///
/// If `table` is not mirrored: a caller names only tables it knows are kept.
pub fn predicate(table: &str) -> &'static str {
    KEPT.iter()
        .find(|k| k.table == table)
        .map_or_else(|| panic!("{table} is not a kept table"), |k| k.predicate)
}

/// The tables that live only in memory: a restart starts them empty, and they leave `state.db` at
/// load. None today: a Claude agent's chat is memory-only through `current_chat!` instead.
pub const VOLATILE: &[&str] = &[];
