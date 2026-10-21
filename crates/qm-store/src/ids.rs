//! Fresh IDs: a random number from a kind's space that no row of its table, and no name the app
//! holds for it, already uses.
//!
//! The numbers aren't counted: nothing is kept to say what was handed out before, so a row
//! forgotten or a folder deleted can't make a number come back to a running app by accident.
//! Names the database doesn't hold (task numbers still on git branches or task folders from an
//! earlier run) are [`reserve`]d at startup; they live in memory only.
//!
//! Both the source and the reservations are per thread: the database thread in the app, which is
//! the only thread that touches the store.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use qm_types::ids::{IdSource, RandomIds, free_number};
use rusqlite::{Connection, params};

use crate::StoreError;

/// Draws before enumerating the free numbers: a space this sparse never needs the scan.
const DRAWS: u32 = 32;

/// Spaces with more numbers than this aren't enumerated; a full one is refused instead.
const ENUMERABLE: u32 = 1_000_000;

thread_local! {
    static SOURCE: RefCell<Option<Box<dyn IdSource>>> = const { RefCell::new(None) };
    static RESERVED: RefCell<BTreeSet<(&'static str, u32)>> = const { RefCell::new(BTreeSet::new()) };
}

/// Uses `source` for every fresh ID on this thread (the app's startup, and tests).
pub fn use_source(source: Box<dyn IdSource>) {
    SOURCE.with_borrow_mut(|slot| *slot = Some(source));
}

/// Keeps `numbers` of `table` out of [`fresh`]: names held outside the database (task numbers
/// still on git branches and folders from an earlier run).
pub fn reserve(table: &'static str, numbers: impl IntoIterator<Item = u32>) {
    RESERVED.with_borrow_mut(|reserved| reserved.extend(numbers.into_iter().map(|n| (table, n))));
}

/// A number from `space` that `table` doesn't hold and no reservation covers.
pub fn fresh(
    conn: &Connection,
    table: &'static str,
    space: RangeInclusive<u32>,
) -> Result<u32, StoreError> {
    SOURCE.with_borrow_mut(|slot| {
        let source = slot.get_or_insert_with(|| Box::new(RandomIds::new()));
        draw(conn, table, &space, source.as_mut())
    })
}

fn draw(
    conn: &Connection,
    table: &'static str,
    space: &RangeInclusive<u32>,
    source: &mut dyn IdSource,
) -> Result<u32, StoreError> {
    for _ in 0..DRAWS {
        let n = source.number(*space.start(), *space.end());
        if is_free(conn, table, n)? {
            return Ok(n);
        }
    }
    if space.end() - space.start() >= ENUMERABLE {
        return Err(full(table, space));
    }
    let taken = taken_in(conn, table, space)?;
    free_number(source, space, &taken).ok_or_else(|| full(table, space))
}

fn is_free(conn: &Connection, table: &'static str, n: u32) -> Result<bool, StoreError> {
    if RESERVED.with_borrow(|reserved| reserved.contains(&(table, n))) {
        return Ok(false);
    }
    let held: bool = conn.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE id = ?1)"),
        params![n],
        |r| r.get(0),
    )?;
    Ok(!held)
}

/// Every number of `table` in `space` that is held or reserved.
fn taken_in(
    conn: &Connection,
    table: &'static str,
    space: &RangeInclusive<u32>,
) -> Result<BTreeSet<u32>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM {table} WHERE id BETWEEN ?1 AND ?2"
    ))?;
    let mut taken = stmt
        .query_map(params![space.start(), space.end()], |r| r.get::<_, u32>(0))?
        .collect::<rusqlite::Result<BTreeSet<u32>>>()?;
    RESERVED.with_borrow(|reserved| {
        taken.extend(
            reserved
                .iter()
                .filter(|(t, n)| *t == table && space.contains(n))
                .map(|(_, n)| *n),
        );
    });
    Ok(taken)
}

fn full(table: &'static str, space: &RangeInclusive<u32>) -> StoreError {
    StoreError::Invalid {
        column: table,
        message: format!(
            "every number from {} to {} is in use, so there is no fresh ID left for {table}",
            space.start(),
            space.end()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Hands out the scripted numbers in order (clamped to the range), then repeats the last one.
    struct Scripted(VecDeque<u32>, u32);

    impl IdSource for Scripted {
        fn number(&mut self, min: u32, max: u32) -> u32 {
            let next = self.0.pop_front().unwrap_or(self.1);
            next.clamp(min, max)
        }
    }

    fn memory() -> Connection {
        let mut conn = Connection::open_in_memory().expect("memory database");
        crate::schema::migrate(&mut conn).expect("schema");
        conn
    }

    #[test]
    fn a_held_number_is_skipped_and_the_next_scripted_one_is_taken() {
        let conn = memory();
        conn.execute(
            "INSERT INTO secondmates (id, name, scope, created_at, seq) VALUES (5, 'a', '', 0, 1)",
            [],
        )
        .expect("a secondmate");
        use_source(Box::new(Scripted(VecDeque::from([5, 6]), 6)));
        let n = fresh(&conn, "secondmates", qm_types::ids::space::SECONDMATE).expect("fresh");
        assert_eq!(n, 6);
    }

    #[test]
    fn a_reserved_task_number_is_skipped() {
        let conn = memory();
        reserve("tasks", [7]);
        use_source(Box::new(Scripted(VecDeque::from([7, 8]), 8)));
        let n = fresh(&conn, "tasks", qm_types::ids::space::TASK).expect("fresh");
        assert_eq!(n, 8);
    }

    #[test]
    fn a_full_small_space_is_refused_with_a_plain_reason() {
        let conn = memory();
        for n in 1..=99u32 {
            conn.execute(
                "INSERT INTO secondmates (id, name, scope, created_at, seq) VALUES (?1, ?2, '', 0, ?1)",
                params![n, format!("s{n}")],
            )
            .expect("fill");
        }
        use_source(Box::new(Scripted(VecDeque::new(), 1)));
        let err = fresh(&conn, "secondmates", qm_types::ids::space::SECONDMATE).expect_err("full");
        assert!(
            err.to_string()
                .contains("every number from 1 to 99 is in use")
        );
    }

    #[test]
    fn rows_read_back_in_creation_order_whatever_their_ids_are() {
        use crate::events::{self, NewEvent};
        let conn = memory();
        // Numbers that fall as the rows are written: the ids run against creation order.
        use_source(Box::new(Scripted(VecDeque::from([900, 800, 700]), 700)));
        for summary in ["first", "second", "third"] {
            events::insert(
                &conn,
                &NewEvent {
                    kind: "test.order",
                    agent: None,
                    task: None,
                    turn: None,
                    summary,
                    payload: serde_json::json!({}),
                    at: jiff::Timestamp::now(),
                },
            )
            .expect("insert");
        }
        let by_creation: Vec<(i64, String)> = conn
            .prepare("SELECT id, summary FROM events WHERE kind = 'test.order' ORDER BY seq")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert_eq!(
            by_creation,
            vec![
                (900, "first".to_owned()),
                (800, "second".to_owned()),
                (700, "third".to_owned()),
            ]
        );
    }

    #[test]
    fn a_free_number_is_found_after_the_draws_collide() {
        let conn = memory();
        for n in 1..=98u32 {
            conn.execute(
                "INSERT INTO secondmates (id, name, scope, created_at, seq) VALUES (?1, ?2, '', 0, ?1)",
                params![n, format!("s{n}")],
            )
            .expect("fill");
        }
        use_source(Box::new(Scripted(VecDeque::new(), 1)));
        let n = fresh(&conn, "secondmates", qm_types::ids::space::SECONDMATE).expect("fresh");
        assert_eq!(n, 99);
    }
}
