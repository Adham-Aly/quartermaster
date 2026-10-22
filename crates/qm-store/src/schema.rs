//! The schema, as migrations.
//!
//! [`MIGRATIONS`] is append-only: a new table or column is a new file `migrations/NNNN_<what>.sql`
//! and one line here, never an edit to an applied migration.
//! `rusqlite_migration` records progress in `PRAGMA user_version`; applying is idempotent.

use crate::StoreError;
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

/// The SQL is shared by the checked working-database migrations and the incomplete disk mirror.
const SQL: &[&str] = &[
    include_str!("../migrations/0001_initial.sql"),
    include_str!("../migrations/0002_crew.sql"),
    include_str!("../migrations/0004_delivery.sql"),
    include_str!("../migrations/0005_verification.sql"),
    include_str!("../migrations/0006_sessions.sql"),
    include_str!("../migrations/0007_tier_entries.sql"),
    include_str!("../migrations/0008_folder.sql"),
    include_str!("../migrations/0009_creation_order.sql"),
    include_str!("../migrations/0010_parent_cursor.sql"),
    include_str!("../migrations/0011_supervisor_payload.sql"),
    include_str!("../migrations/0012_forgotten_parent_facts.sql"),
];

/// Every migration, in order.
pub static MIGRATIONS: &[M<'static>] = &[
    M::up(SQL[0]).foreign_key_check(),
    M::up(SQL[1]).foreign_key_check(),
    M::up(SQL[2]).foreign_key_check(),
    M::up(SQL[3]).foreign_key_check(),
    M::up(SQL[4]).foreign_key_check(),
    M::up(SQL[5]).foreign_key_check(),
    M::up(SQL[6]).foreign_key_check(),
    M::up(SQL[7]).foreign_key_check(),
    M::up(SQL[8]).foreign_key_check(),
    M::up(SQL[9]).foreign_key_check(),
    M::up(SQL[10]).foreign_key_check(),
];

/// Upgrades an intentionally incomplete mirror. Its foreign keys are repaired and validated only
/// after kept rows have been loaded into the working database (`mirror::load`). Checking them
/// during a disk migration would reject references to deliberately unmirrored conversation rows.
pub(crate) fn migrate_mirror(conn: &mut Connection) -> Result<(), StoreError> {
    Migrations::new(SQL.iter().map(|sql| M::up(sql)).collect()).to_latest(conn)?;
    Ok(())
}

/// The migration set.
pub fn migrations() -> Migrations<'static> {
    Migrations::from_slice(MIGRATIONS)
}

/// Brings a configured connection's database up to date. Already-applied migrations are skipped, so
/// re-running is a no-op. Startup runs this on the database thread.
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrations().to_latest(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database as it was before creation order: every table up to migration 0007, with an
    /// event that has an ID but no `seq`.
    fn before_creation_order() -> Connection {
        let mut conn = Connection::open_in_memory().expect("memory database");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        Migrations::from_slice(&MIGRATIONS[..5])
            .to_latest(&mut conn)
            .expect("the old schema");
        conn.execute(
            "INSERT INTO events (at, kind, summary, payload) VALUES (1, 'a', 'first', '{}'),
                                                                    (2, 'b', 'second', '{}')",
            [],
        )
        .expect("old events");
        conn
    }

    #[test]
    fn creation_order_backfills_seq_from_ids_and_refuses_event_updates() {
        let mut conn = before_creation_order();
        migrate(&mut conn).expect("the upgrade");

        let pairs: Vec<(i64, i64)> = conn
            .prepare("SELECT id, seq FROM events ORDER BY id")
            .expect("query")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("values");
        assert_eq!(
            pairs,
            vec![(1, 1), (2, 2)],
            "an upgraded row keeps its ID as its seq"
        );

        let update = conn.execute("UPDATE events SET summary = 'changed'", []);
        assert!(update.is_err(), "events are never updated");
        // Migration 0009 drops the delete guard: forgetting and the disk mirror remove events
        // (plan 3.3), so the guard is the store's, not the schema's.
        let delete = conn.execute("DELETE FROM events", []);
        assert_eq!(delete.expect("a delete"), 2, "events can be forgotten");
    }

    #[test]
    fn an_upgrade_names_the_supervisor_on_pre_upgrade_task_events_only() {
        let mut conn = Connection::open_in_memory().expect("memory database");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        // Through 0010, before 0011 added supervisors to event payloads.
        Migrations::from_slice(&MIGRATIONS[..9])
            .to_latest(&mut conn)
            .expect("the old schema");
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                 merge_method, created_at, seq) \
             VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
             INSERT INTO agents (id, role, home, status, created_at, seq) \
             VALUES (1, 'quartermaster', '/home', 'idle', 1, 1);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, \
                 branch, status, outcome, created_at, closed_at, seq) \
             VALUES (1, 1, 1, 'build', 't', 'g', 'direct_pr', 'qm/task-1', 'closed', 'landed', 1, 1, 1);
             INSERT INTO events (id, seq, at, kind, task_id, summary, payload) VALUES \
                 (1, 1, 1, 'task.created', 1, 'made', '{\"goal\":\"g\"}'),
                 (2, 2, 2, 'agent.started', NULL, 'started', '{}'),
                 (3, 3, 3, 'task.closed', 1, 'closed', '{\"supervisor\":1,\"outcome\":\"landed\"}');",
        )
        .expect("the old rows");

        migrate(&mut conn).expect("the upgrade");

        let payloads: Vec<serde_json::Value> = conn
            .prepare("SELECT payload FROM events ORDER BY seq")
            .expect("query")
            .query_map([], |r| r.get::<_, String>(0))
            .expect("rows")
            .map(|p| serde_json::from_str(&p.expect("a payload")).expect("json"))
            .collect();
        assert_eq!(
            payloads,
            vec![
                serde_json::json!({"goal": "g", "supervisor": 1}),
                serde_json::json!({}),
                serde_json::json!({"supervisor": 1, "outcome": "landed"}),
            ],
            "a task event gains its task's supervisor; other events are left alone"
        );
        let update = conn.execute("UPDATE events SET summary = 'changed'", []);
        assert!(
            update.is_err(),
            "the upgrade puts the append-only guard back"
        );
    }

    #[test]
    fn the_kept_mirror_needs_references_that_may_be_forgotten() {
        let mut conn = Connection::open_in_memory().expect("memory database");
        migrate(&mut conn).expect("the schema");

        let nullable = |table: &str, column: &str| -> bool {
            let not_null: i64 = conn
                .query_row(
                    "SELECT \"notnull\" FROM pragma_table_info(?1) WHERE name = ?2",
                    [table, column],
                    |r| r.get(0),
                )
                .expect("a column");
            not_null == 0
        };
        assert!(nullable("standing_instructions", "message_id"));
        assert!(nullable("standing_instructions", "turn_id"));
        assert!(nullable("secondmate_requests", "turn_id"));

        let task_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list('tasks')
                 WHERE \"from\" IN ('builds_on', 'parent_task_id', 'retry_of')",
                [],
                |r| r.get(0),
            )
            .expect("foreign keys");
        assert_eq!(task_refs, 0, "a task may name a task that is forgotten");

        let watermarks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'block_watermarks'",
                [],
                |r| r.get(0),
            )
            .expect("the table list");
        assert_eq!(watermarks, 1);
    }
}
