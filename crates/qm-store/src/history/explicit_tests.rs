use super::tests::{count, working};
use super::*;
use crate::parent_channel;
use qm_types::ids::EventId;

#[test]
fn pending_reports_survive_cap_and_explicit_forgetting_until_dismissed() {
    for explicit in [false, true] {
        for kind in ["lookup", "research", "reproduce", "teach", "build"] {
            for (reported, dismissed, outcome, pending) in [
                (Some(20), None, "answered", true),
                (Some(20), Some(10), "answered", true),
                (Some(20), Some(20), "answered", false),
                (Some(20), Some(30), "answered", false),
                (None, None, "answered", false),
                (Some(20), None, "failed", false),
            ] {
                let mut conn = working();
                conn.execute(
                    "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                         delivery_mode, status, outcome, created_at, closed_at, reported_at,
                         report_read_at, seq)
                     VALUES (1, 1, 1, ?1, 'report', 'goal', 'folder', 'closed', ?2, 1, 30, ?3, ?4, 1)",
                    params![kind, outcome, reported, dismissed],
                ).expect("closed task");
                conn.execute_batch(
                    "INSERT INTO reports (id, task_id, agent_id, source, final_text, created_at, seq)
                     VALUES (1, 1, 1, 'final_text', 'answer', 20, 1);",
                ).expect("report");
                let forgotten = if explicit {
                    forget_closed_task_details(&mut conn)
                } else {
                    forget_closed_tasks(&mut conn, 0)
                }
                .expect("forget");
                let protected = pending && kind != "build";
                assert_eq!(
                    forgotten.is_empty(),
                    protected,
                    "{kind}, {reported:?}, {dismissed:?}, {outcome}, explicit={explicit}"
                );
                assert_eq!(count(&conn, "reports"), i64::from(protected));
                assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
            }
        }
    }
}

#[test]
fn forgetting_removes_only_the_deleted_held_actions_unowned_card_links() {
    for explicit in [false, true] {
        let mut conn = routed_history();
        conn.execute_batch(
            "INSERT INTO held_actions (id, kind, payload, title, task_id, asked_by, created_at, seq) VALUES
                 (1, 'decision', '{\"kind\":\"decision\"}', 'closed task', 1, 1, 0, 1),
                 (2, 'decision', '{\"kind\":\"decision\"}', 'crew owned', NULL, 3, 0, 2),
                 (3, 'decision', '{\"kind\":\"decision\"}', 'open task', 3, 1, 0, 3);
             INSERT INTO events (id, at, kind, summary, payload, seq) VALUES
                 (50, 0, 'held.shown', '', '{\"held\":1,\"message\":100}', 50),
                 (51, 0, 'held.shown', '', '{\"held\":2,\"message\":101}', 51),
                 (52, 0, 'held.shown', '', '{\"held\":3,\"message\":102}', 52);",
        ).expect("unowned card anchors for task- and agent-owned held actions");
        if explicit {
            forget_closed_task_details(&mut conn).expect("clear");
        } else {
            parent_channel::set_cursor(&conn, 999).expect("pump caught up");
            forget_closed_tasks(&mut conn, 0).expect("cap eviction");
        }
        assert_eq!(count(&conn, "held_actions"), 1);
        assert_eq!(count(&conn, "events WHERE kind = 'held.shown'"), 1);
        assert_eq!(count(&conn, "events WHERE id = 52"), 1);
        assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
    }
}

#[test]
fn shared_request_keeps_judgments_and_sibling_deliveries_but_forgets_owned_facts() {
    for explicit in [false, true] {
        let mut conn = routed_history();
        conn.execute_batch(
            "INSERT INTO events (id, at, kind, task_id, summary, payload, seq) VALUES
                 (30, 0, 'task.started', 3, 'open started', '{}', 30),
                 (31, 0, 'secondmate.facts', 2, 'closed fact', '{\"row\":3}', 31),
                 (32, 0, 'secondmate.facts', 3, 'open fact', '{\"row\":4}', 32);
             INSERT INTO events (id, at, kind, summary, payload, seq) VALUES
                 (33, 0, 'secondmate.report', 'shared judgment', '{\"row\":2}', 33);
             INSERT INTO parent_channel (id, secondmate_id, correlation_id, facts, created_at, seq) VALUES
                 (3, 1, 2, '{\"source_event\":11}', 0, 3),
                 (4, 1, 2, '{\"source_event\":30}', 0, 4);
             INSERT INTO wakes (id, target_agent_id, kind, payload, dedupe_key, task_id, created_at, seq) VALUES
                 (100, 1, 'parent_report', '{\"text\":\"shared judgment\",\"correlation\":\"R-0002\"}',
                      'parent:R-0002:2', NULL, 0, 100),
                 (101, 1, 'parent_report', '{\"text\":\"closed fact\",\"correlation\":\"R-0002\"}',
                      'parent:R-0002:3', 2, 0, 101),
                 (102, 1, 'parent_report', '{\"text\":\"open fact\",\"correlation\":\"R-0002\"}',
                      'parent:R-0002:4', 3, 0, 102);",
        ).expect("shared request judgment and pumped sibling facts");
        if explicit {
            forget_closed_task_details(&mut conn).expect("clear");
        } else {
            parent_channel::set_cursor(&conn, 999).expect("pump caught up");
            forget_closed_tasks(&mut conn, 0).expect("cap eviction");
        }
        assert_eq!(count(&conn, "parent_channel WHERE id IN (2, 4)"), 2);
        assert_eq!(count(&conn, "parent_channel WHERE id IN (1, 3)"), 0);
        assert_eq!(count(&conn, "events WHERE id IN (30, 32, 33)"), 3);
        assert_eq!(count(&conn, "events WHERE id = 31"), 0);
        assert_eq!(
            count(
                &conn,
                "wakes WHERE id IN (100, 102) AND consumed_at IS NULL"
            ),
            2
        );
        assert_eq!(count(&conn, "wakes WHERE id = 101 AND task_id IS NULL"), 1);
        assert!(parent_channel::has_source_event(&conn, EventId::new(11)).expect("dedupe"));
        assert_eq!(count(&conn, "secondmate_requests WHERE id = 2"), 1);
        assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
    }
}

fn routed_history() -> Connection {
    let conn = working();
    conn.execute_batch(
        "INSERT INTO secondmates (id, name, scope, created_at, seq) VALUES (1, 'Web', 'web', 0, 1);
         INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, created_at, seq)
             VALUES (2, 'secondmate', 1, 1, '/sm', 'idle', 0, 2);
         INSERT INTO secondmate_requests (id, secondmate_id, request, status, created_at, seq)
             VALUES (1, 1, 'PRIVATE request', 'answered', 0, 1),
                    (2, 1, 'active shared request', 'open', 0, 2);
         INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode,
             status, outcome, closed_at, secondmate_request_id, created_at, seq) VALUES
             (1, 1, 2, 'build', 'PRIVATE title', 'PRIVATE goal', 'folder', 'closed', 'answered', 1, 1, 0, 1),
             (2, 1, 2, 'build', 'PRIVATE title', 'PRIVATE goal', 'folder', 'closed', 'answered', 1, 2, 0, 2),
             (3, 1, 2, 'build', 'open task', 'open goal', 'folder', 'queued', NULL, NULL, 2, 0, 3);
         INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
             VALUES (3, 'crewmate', 2, 1, '/crew', 'stopped', 0, 3);
         INSERT INTO turns (id, agent_id, started_by, started_at, ended_at, status, seq)
             VALUES (1, 3, 'app_event', 0, 1, 'completed', 1);
         INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq)
             VALUES (1, 3, 'agent', 'message', 'PRIVATE crew history', 0, 1);
         INSERT INTO session_lines (agent_id, turn_id, kind, text, created_at, seq)
             VALUES (3, 1, 'agent', 'PRIVATE crew line', 0, 1);
         INSERT INTO reports (id, task_id, agent_id, source, outcome, answer, created_at, seq)
             VALUES (1, 1, 3, 'report_call', 'done', 'PRIVATE answer', 0, 1);
         INSERT INTO parent_channel_cursor (seq, event_seq) VALUES (1, 0);
         INSERT INTO block_watermarks (agent_id, event_seq, seq) VALUES (1, 0, 1);
         INSERT INTO events (id, at, kind, task_id, summary, payload, seq) VALUES
             (10, 0, 'task.closed', 1, 'PRIVATE summary', '{\"outcome\":\"answered\",\"text\":\"PRIVATE tool\"}', 10),
             (11, 0, 'task.reported', 2, 'PRIVATE report', '{\"outcome\":\"done\",\"text\":\"PRIVATE report\"}', 11),
             (12, 0, 'tool.result', 1, 'PRIVATE tool', '{\"text\":\"PRIVATE tool\"}', 12);
         INSERT INTO parent_channel (id, secondmate_id, correlation_id, facts, judgment, created_at, seq)
             VALUES (1, 1, 1, '{}', '{\"summary\":\"PRIVATE judgment\"}', 0, 1),
                    (2, 1, 2, '{}', '{\"summary\":\"PRIVATE judgment\"}', 0, 2);",
    ).expect("routed closed tasks with unread facts and an open dependent");
    conn
}

#[test]
fn explicit_forgetting_discards_details_without_waiting_for_the_pump() {
    let mut conn = routed_history();
    // Simulate the pump's read/record split: this batch has already escaped the DB thread.
    let scan = parent_channel::events_after(&conn, 0, &["task.closed", "task.reported"], 10)
        .expect("pump batch");
    assert_eq!(scan.rows.len(), 2);
    let mut ids = forget_closed_task_details(&mut conn).expect("explicit clear");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
    for table in ["reports", "messages", "turns", "session_lines", "events"] {
        assert_eq!(
            count(&conn, table),
            0,
            "{table} details must go despite zero watermarks"
        );
    }
    assert_eq!(count(&conn, "parent_channel WHERE id = 1"), 0);
    assert_eq!(
        count(
            &conn,
            "parent_channel WHERE id = 2 AND judgment IS NOT NULL"
        ),
        1,
        "the retained sibling's request-wide judgment stays"
    );
    assert_eq!(count(&conn, "agents WHERE role = 'crewmate'"), 0);
    assert_eq!(count(&conn, "tasks"), 1);
    assert_eq!(count(&conn, "secondmate_requests WHERE id = 1"), 0);
    assert_eq!(
        count(&conn, "secondmate_requests WHERE id = 2"),
        1,
        "open task's request stays"
    );
    assert_eq!(
        count(&conn, "wakes WHERE task_id IS NULL AND consumed_at IS NULL"),
        2
    );
    assert_eq!(count(&conn, "wakes WHERE payload LIKE '%PRIVATE%'"), 0);
    for event in scan.rows {
        assert!(parent_channel::has_source_event(&conn, event.id).expect("dedupe stale batch"));
    }
    assert!(
        ids::fresh(&conn, "events", 10..=11).is_err(),
        "source IDs cannot be reused"
    );
    assert_eq!(
        parent_channel::cursor(&conn).expect("cursor"),
        Some(0),
        "no other task's facts skipped"
    );
    assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
    assert_eq!(
        forget_closed_task_details(&mut conn).expect("repeat clear"),
        [] as [u32; 0]
    );
    assert_eq!(count(&conn, "wakes"), 2, "no duplicate delivery");
}

/// The wakes here were written before correlation IDs read `R-`.
#[test]
fn already_pumped_pending_fact_is_detached_not_duplicated_or_discarded() {
    let mut conn = routed_history();
    conn.execute_batch(
        "INSERT INTO parent_channel (id, secondmate_id, correlation_id, facts, created_at, seq)
             VALUES (3, 1, 1, '{\"source_event\":10}', 0, 3);
         INSERT INTO wakes (id, target_agent_id, kind, payload, dedupe_key, task_id, created_at, seq)
             VALUES (100, 1, 'parent_report', '{\"text\":\"T-0001 closed (answered).\",\"correlation\":\"C-0001\"}',
                     'parent:C-0001:3', 1, 0, 100),
                    (101, 1, 'parent_report', '{\"text\":\"PRIVATE judgment\",\"correlation\":\"C-0001\"}',
                     'parent:C-0001:1', 1, 0, 101);",
    ).expect("pump row, fact wake and judgment wake");
    forget_closed_task_details(&mut conn).expect("forget");
    assert_eq!(count(&conn, "wakes"), 2);
    assert_eq!(count(&conn, "wakes WHERE id = 100 AND task_id IS NULL"), 1);
    assert_eq!(count(&conn, "wakes WHERE id = 101"), 0);
    assert!(parent_channel::has_source_event(&conn, EventId::new(10)).expect("replay guard"));
    assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
}

#[test]
fn detached_delivery_survives_mirror_restart_without_restoring_details() {
    let dir = std::env::temp_dir().join(format!("qm-r12-forgetting-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test folder");
    let path = dir.join("state.db");
    let _ = std::fs::remove_file(&path);
    let fixture = routed_history();
    fixture
        .execute("VACUUM INTO ?1", [path.to_str().expect("path")])
        .expect("fixture snapshot");
    let (mut conn, mut mirror) = crate::mirror::Mirror::open(&path).expect("load fixture");
    forget_closed_task_details(&mut conn).expect("forget");
    mirror.flush(&conn).expect("flush");
    drop(conn);
    drop(mirror);
    let (conn, mut mirror) = crate::mirror::Mirror::open(&path).expect("restart");
    assert_eq!(count(&conn, "tasks"), 1);
    assert_eq!(count(&conn, "reports"), 0);
    assert_eq!(count(&conn, "events"), 0);
    assert_eq!(
        count(&conn, "wakes WHERE task_id IS NULL AND consumed_at IS NULL"),
        2
    );
    assert_eq!(count(&conn, "forgotten_parent_facts"), 2);
    assert_eq!(count(&conn, "secondmate_requests"), 1);
    assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
    conn.execute_batch(
        "UPDATE wakes SET delivered_at = 1, delivered_turn_id = 77, consumed_at = 1;",
    )
    .expect("consume delivered facts");
    assert!(parent_channel::has_source_event(&conn, EventId::new(10)).expect("run-long dedupe"));
    mirror.flush(&conn).expect("flush consumed delivery");
    let disk = crate::connection::open(&path).expect("disk");
    assert_eq!(
        count(&disk, "forgotten_parent_facts"),
        0,
        "consumed markers need no disk history"
    );
    drop(disk);
    drop(mirror);
    drop(conn);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn minimal_recent_outcomes_survive_but_do_not_retain_old_detail_text() {
    let mut conn = routed_history();
    conn.execute_batch(
        "INSERT INTO events (id, at, kind, task_id, summary, payload, seq)
         VALUES (20, 0, 'outcome.landed', 1, 'PRIVATE old title and destination',
                 '{\"supervisor\":2,\"text\":\"PRIVATE tool\",\"branch\":\"PRIVATE branch\"}', 20);",
    ).expect("minimal outcome candidate");
    forget_closed_task_details(&mut conn).expect("forget");
    assert_eq!(
        count(
            &conn,
            "events WHERE kind = 'outcome.landed' AND task_id IS NULL"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "events WHERE summary LIKE '%PRIVATE%' OR payload LIKE '%PRIVATE%'"
        ),
        0
    );
    let payload: String = conn
        .query_row("SELECT payload FROM events", [], |r| r.get(0))
        .expect("payload");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).expect("json"),
        serde_json::json!({"supervisor": 2})
    );
}

#[test]
fn before_pump_start_only_closed_task_facts_are_delivered_and_open_facts_remain_scannable() {
    let mut conn = routed_history();
    conn.execute_batch(
        "DELETE FROM parent_channel_cursor;
         INSERT INTO events (id, at, kind, task_id, summary, payload, seq)
             VALUES (30, 0, 'task.started', 3, 'open started', '{}', 30);",
    )
    .expect("pump not yet started");
    forget_closed_task_details(&mut conn).expect("forget");
    let scan = parent_channel::events_after(&conn, 0, &["task.started", "task.closed"], 10)
        .expect("remaining pump batch");
    assert_eq!(scan.rows.len(), 1);
    assert_eq!(scan.rows[0].id, EventId::new(30));
    assert_eq!(count(&conn, "wakes"), 2);
    assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
}

#[test]
fn a_failed_delivery_rolls_back_the_forgetting_too() {
    let mut conn = routed_history();
    conn.execute_batch(
        "CREATE TEMP TRIGGER refuse_wake BEFORE INSERT ON wakes BEGIN SELECT RAISE(ABORT, 'no'); END;",
    )
    .expect("failure injection");
    assert!(forget_closed_task_details(&mut conn).is_err());
    assert_eq!(count(&conn, "tasks"), 3);
    assert_eq!(count(&conn, "reports"), 1);
    assert_eq!(count(&conn, "events"), 3);
    assert_eq!(count(&conn, "parent_channel"), 2);
    assert_eq!(count(&conn, "forgotten_parent_facts"), 0);
    conn.execute_batch("DROP TRIGGER refuse_wake;")
        .expect("allow delivery");
    forget_closed_task_details(&mut conn).expect("retry");
    assert_eq!(count(&conn, "tasks"), 1);
}

#[test]
fn request_only_history_uses_display_correlations_and_shared_requests_stay() {
    let mut conn = routed_history();
    conn.execute_batch(
        "INSERT INTO events (id, at, kind, summary, payload, seq) VALUES
             (40, 0, 'secondmate.routed', 'PRIVATE exclusive request', '{\"correlation\":\"R-0001\"}', 40),
             (41, 0, 'secondmate.routed', 'active shared request', '{\"correlation\":\"R-0002\"}', 41);
         INSERT INTO wakes (id, target_agent_id, kind, payload, dedupe_key, created_at, seq)
             VALUES (100, 1, 'parent_report', '{\"text\":\"PRIVATE judgment\",\"correlation\":\"R-0001\"}',
                     'parent:R-0001:1', 0, 100),
                    (101, 2, 'routed', '{\"text\":\"active request\",\"correlation\":\"R-0002\"}',
                     'routed:R-0002', 0, 101);",
    ).expect("typed request history without task references");
    forget_closed_task_details(&mut conn).expect("forget");
    assert_eq!(count(&conn, "events WHERE id = 40"), 0);
    assert_eq!(count(&conn, "events WHERE id = 41"), 1);
    assert_eq!(count(&conn, "wakes WHERE id = 100"), 0);
    assert_eq!(
        count(&conn, "wakes WHERE id = 101"),
        1,
        "open request delivery stays"
    );
    assert_eq!(count(&conn, "wakes"), 3);
    assert_eq!(count(&conn, "secondmate_requests WHERE id = 2"), 1);
    assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
}

#[test]
fn cap_eviction_does_not_orphan_details_that_a_later_clear_cannot_identify() {
    let mut conn = routed_history();
    parent_channel::set_cursor(&conn, 999).expect("pump has read task facts");
    forget_closed_tasks(&mut conn, 0).expect("cap eviction");
    assert_eq!(count(&conn, "events"), 0);
    assert_eq!(count(&conn, "parent_channel"), 1);
    assert_eq!(count(&conn, "parent_channel WHERE id = 2"), 1);
    assert_eq!(count(&conn, "secondmate_requests WHERE id = 1"), 0);
    assert_eq!(count(&conn, "secondmate_requests WHERE id = 2"), 1);
    forget_closed_task_details(&mut conn).expect("later clear");
    assert_eq!(count(&conn, "pragma_foreign_key_check"), 0);
}
