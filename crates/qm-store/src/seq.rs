//! Creation order: every row's `seq`.
//!
//! `seq` is the row's creation instant in microseconds, made strictly increasing: a new row gets
//! `max(its instant, last seq + 1)`, so a clock that steps back still sorts rows in the order they
//! were made. Nothing about the sequence is stored on its own: `last` lives on the database thread
//! and starts at the largest `seq` of the rows loaded at startup ([`observe`]). Because it is
//! derived from a time rather than counted, deleting rows never lets a later row reuse a `seq` an
//! old reference still names, which is why it isn't a counter.
//!
//! Ordering (`ORDER BY`, `>`/`<` cursors, "latest") uses `seq`, never the random `id` (`ids.rs`).

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension};
use std::cell::Cell;

thread_local! {
    /// The last `seq` handed out on this thread (the database thread in the app).
    static LAST: Cell<i64> = const { Cell::new(0) };
}

/// The `seq` for a row created at `at`.
pub fn next(at: Timestamp) -> i64 {
    next_micros(at.as_microsecond())
}

/// The `seq` for a row created at `millis` (the `*_at` columns are milliseconds).
pub fn next_millis(millis: i64) -> i64 {
    next_micros(millis.saturating_mul(1000))
}

fn next_micros(micros: i64) -> i64 {
    LAST.with(|last| {
        let seq = micros.max(last.get().saturating_add(1));
        last.set(seq);
        seq
    })
}

/// Records a `seq` already in the database (at load), so later rows sort after it.
pub fn observe(seq: i64) {
    LAST.with(|last| last.set(last.get().max(seq)));
}

/// Records the largest `seq` of every table in `conn`. Call it on the thread that will write
/// (the clock is per thread), before it writes anything: after a restart, a clock that has
/// stepped back must not number a new row below a loaded one.
pub fn observe_all(conn: &Connection) -> Result<(), crate::StoreError> {
    for table in TABLES {
        let newest: Option<i64> =
            conn.query_row(&format!("SELECT MAX(seq) FROM {table}"), [], |r| r.get(0))?;
        if let Some(seq) = newest {
            observe(seq);
        }
    }
    // The events at these positions may already have been pruned. Their durable cursors still
    // order future events, even when the clock has moved backwards.
    let position: Option<i64> = conn.query_row(
        "SELECT MAX(position) FROM (SELECT event_seq AS position FROM block_watermarks \
         UNION ALL SELECT prev_event_seq FROM block_watermarks \
         UNION ALL SELECT event_seq FROM parent_channel_cursor)",
        [],
        |r| r.get(0),
    )?;
    if let Some(seq) = position {
        observe(seq);
    }
    Ok(())
}

/// The tables that carry a `seq`, one per row kind. Every table in the schema has one.
pub const TABLES: &[&str] = &[
    "projects",
    "accounts",
    "models",
    "placements",
    "secondmates",
    "secondmate_projects",
    "agents",
    "messages",
    "user_message_sources",
    "turns",
    "secondmate_requests",
    "tasks",
    "routing_decisions",
    "task_links",
    "reports",
    "held_actions",
    "wakes",
    "events",
    "parent_channel",
    "standing_instructions",
    "schedules",
    "deferred_starts",
    "jobs",
    "processes",
    "session_lines",
    "block_watermarks",
    "parent_channel_cursor",
    "forgotten_parent_facts",
];

/// The `seq` of the row `id` in `table`. A cursor names its row by ID and is compared by position,
/// so this turns one into the other; if the row is gone, `missing` (the edge the caller means).
pub fn of(conn: &Connection, table: &'static str, id: i64, missing: i64) -> rusqlite::Result<i64> {
    let found = conn
        .query_row(
            &format!("SELECT seq FROM {table} WHERE id = ?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.unwrap_or(missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_that_steps_back_still_sorts_rows_in_creation_order() {
        let first = next_micros(5_000_000);
        let stepped_back = next_micros(4_000_000);
        let later = next_micros(4_000_000);
        assert!(first < stepped_back && stepped_back < later);
    }

    #[test]
    fn startup_observes_durable_positions_even_when_the_events_are_gone() {
        // Isolate each maximum so forgetting any one durable column breaks the regression.
        for column in ["seq", "event_seq", "prev_event_seq", "cursor"] {
            LAST.with(|last| last.set(0));
            let mut conn = crate::connection::open_memory().expect("memory");
            crate::schema::migrate(&mut conn).expect("schema");
            conn.execute_batch(
                "INSERT INTO agents (id, role, home, status, created_at, seq) \
                     VALUES (1, 'quartermaster', '/unused', 'idle', 1, 1);
                 INSERT INTO block_watermarks (agent_id, event_seq, prev_event_seq, seq) \
                     VALUES (1, 10000000, 9999999, 10000001);
                 INSERT INTO parent_channel_cursor (seq, event_seq) VALUES (1, 10000002);",
            )
            .expect("reviewer fixture");
            let high = 20_000_000;
            if column == "cursor" {
                conn.execute("UPDATE parent_channel_cursor SET event_seq = ?1", [high])
                    .expect("cursor maximum");
            } else {
                conn.execute(
                    &format!("UPDATE block_watermarks SET {column} = ?1"),
                    [high],
                )
                .expect("watermark maximum");
            }
            observe_all(&conn).expect("startup observation");
            assert!(next_millis(1) > high, "must observe {column}");
        }
    }

    #[test]
    fn startup_observes_user_sources_after_their_messages_are_gone() {
        LAST.with(|last| last.set(0));
        let mut conn = crate::connection::open_memory().expect("memory");
        crate::schema::migrate(&mut conn).expect("schema");
        conn.execute_batch(
            "INSERT INTO agents (id, role, home, status, created_at, seq) \
                 VALUES (1, 'quartermaster', '/unused', 'idle', 1, 1);
             INSERT INTO user_message_sources (id, agent_id, text, created_at, seq) \
                 VALUES (1, 1, 'Use high effort', 1, 10000000);",
        )
        .expect("durable source without chat");
        observe_all(&conn).expect("startup observation");
        assert!(next_millis(1) > 10_000_000);
    }

    #[test]
    fn observed_seqs_are_never_reused() {
        observe(9_000_000_000_000);
        assert!(next_micros(1) > 9_000_000_000_000);
    }
}
