//! What `state.db` keeps: the rows of each table that outlive the process (plan section 2).
//!
//! The working database lives in memory (`mirror.rs`), and `state.db` mirrors the rows that
//! satisfy each table's predicate here. Every table is in exactly one of [`KEPT`] or [`VOLATILE`],
//! and a test holds that. A predicate may look at other tables, so the order of [`KEPT`] matters
//! when rows are pruned at load: a table's predicate sees the tables before it, already pruned.
//! That is why `agents` comes before `block_watermarks`, and `events` last.
//!
//! The predicates use `COALESCE(…, 0)` in SQL: a NULL result keeps nothing.

/// One mirrored table and the predicate that decides which of its rows are kept.
#[derive(Debug, Clone, Copy)]
pub struct Kept {
    pub table: &'static str,
    /// SQL over the table's own columns, true for the rows to keep.
    pub predicate: &'static str,
}

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
    Kept {
        table: "projects",
        predicate: "removed_at IS NULL OR id IN (SELECT project_id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "secondmates",
        predicate: "retired_at IS NULL",
    },
    Kept {
        table: "secondmate_projects",
        predicate: "secondmate_id IN (SELECT id FROM secondmates WHERE retired_at IS NULL) \
                    AND project_id IN (SELECT id FROM projects \
                    WHERE removed_at IS NULL OR id IN (SELECT project_id FROM tasks WHERE status <> 'closed'))",
    },
    Kept {
        table: "agents",
        predicate: "retired_at IS NULL \
                    OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed') \
                    OR id IN (SELECT supervisor_id FROM tasks WHERE status <> 'closed') \
                    OR id IN (SELECT target_agent_id FROM wakes WHERE consumed_at IS NULL) \
                    OR id IN (SELECT agent_id FROM reports \
                              WHERE task_id IN (SELECT id FROM tasks WHERE status <> 'closed')) \
                    OR id IN (SELECT owner_agent_id FROM schedules WHERE status = 'pending') \
                    OR id IN (SELECT asked_by FROM held_actions WHERE status = 'pending') \
                    OR id IN (SELECT agent_id FROM routing_decisions \
                              WHERE task_id IN (SELECT id FROM tasks WHERE status <> 'closed')) \
                    OR id IN (SELECT agent_id FROM jobs \
                              WHERE task_id IN (SELECT id FROM tasks WHERE status <> 'closed'))",
    },
    Kept {
        table: "tasks",
        predicate: "status <> 'closed'",
    },
    Kept {
        table: "task_links",
        predicate: "blocker IN (SELECT id FROM tasks WHERE status <> 'closed') \
                    AND blocked IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "reports",
        predicate: "task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "held_actions",
        predicate: "status = 'pending' \
                    OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
    },
    Kept {
        table: "wakes",
        predicate: "consumed_at IS NULL \
                    OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed')",
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
        table: "secondmate_requests",
        predicate: "secondmate_id IN (SELECT id FROM secondmates WHERE retired_at IS NULL) \
                    AND (status = 'open' \
                         OR id IN (SELECT secondmate_request_id FROM tasks WHERE status <> 'closed'))",
    },
    Kept {
        table: "parent_channel",
        predicate: "correlation_id IN (SELECT id FROM secondmate_requests)",
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
        table: "events",
        // Block lines not yet seen by the oldest live orchestrator; the newest five outcomes
        // (per secondmate once `payload.supervisor` partitions them, step E); everything of an
        // open task; and the two facts the app keeps for good.
        predicate: "(kind LIKE 'you.%' OR kind LIKE 'task.%' OR kind LIKE 'pr.%' \
                     OR kind LIKE 'checks.%' OR kind LIKE 'secondmate.%' OR kind LIKE 'model.%' \
                     OR kind IN ('model_new', 'model_gone')) \
                    AND seq > (SELECT COALESCE(MIN(event_seq), 0) FROM block_watermarks) \
                    OR seq IN (SELECT seq FROM events AS newest WHERE newest.kind LIKE 'outcome.%' \
                               ORDER BY newest.seq DESC LIMIT 5) \
                    OR task_id IN (SELECT id FROM tasks WHERE status <> 'closed') \
                    OR kind IN ('starter_tiers_seeded', 'settings.model_listed')",
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

/// The tables that live only in memory: a restart starts them empty. The conversation and what
/// the block shows since a turn are here; they leave `state.db` at load.
pub const VOLATILE: &[&str] = &["messages", "session_lines", "turns"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use rusqlite::Connection;

    fn migrated() -> Connection {
        let mut conn = Connection::open_in_memory().expect("memory database");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        schema::migrate(&mut conn).expect("the schema");
        conn
    }

    fn tables(conn: &Connection) -> Vec<String> {
        conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name <> 'schema_migrations'",
        )
        .expect("query")
        .query_map([], |r| r.get(0))
        .expect("rows")
        .collect::<rusqlite::Result<_>>()
        .expect("names")
    }

    #[test]
    fn every_table_is_kept_or_volatile_and_never_both() {
        let conn = migrated();
        let mut all = tables(&conn);
        all.sort();
        let mut classified: Vec<String> = KEPT
            .iter()
            .map(|k| k.table.to_owned())
            .chain(VOLATILE.iter().map(|t| (*t).to_owned()))
            .collect();
        classified.sort();
        assert_eq!(classified, all, "a new table is listed in KEPT or VOLATILE");
        let unique: std::collections::BTreeSet<_> = classified.iter().collect();
        assert_eq!(unique.len(), classified.len(), "a table is listed once");
    }

    #[test]
    fn every_kept_table_has_a_unique_seq_and_a_valid_predicate() {
        let conn = migrated();
        for kept in KEPT {
            let unique_seq: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_index_list(?1) il \
                     JOIN pragma_index_info(il.name) ii \
                     WHERE il.\"unique\" = 1 AND ii.name = 'seq'",
                    [kept.table],
                    |r| r.get(0),
                )
                .expect("index list");
            assert_eq!(unique_seq, 1, "{} has a unique seq", kept.table);
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {} WHERE COALESCE(({}), 0)",
                        kept.table, kept.predicate
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or_else(|e| panic!("{} predicate: {e}", kept.table));
            assert!(count >= 0);
        }
    }
}
