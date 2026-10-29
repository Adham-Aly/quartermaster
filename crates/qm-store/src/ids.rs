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
use std::collections::{BTreeMap, BTreeSet};
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
    /// Tables whose numbers can't be issued, and why: a reservation scan that failed leaves the
    /// numbers it would have reserved unknown.
    static HELD: RefCell<BTreeMap<&'static str, String>> = const { RefCell::new(BTreeMap::new()) };
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

/// Stops issuing `table`'s numbers until the app restarts: the numbers its existing data names
/// couldn't be read, so any number might be one of them. Creating a row that needs one fails.
pub fn hold(table: &'static str, reason: String) {
    HELD.with_borrow_mut(|held| {
        held.entry(table).or_insert(reason);
    });
}

/// A number from `space` that `table` doesn't hold and no reservation covers.
pub fn fresh(
    conn: &Connection,
    table: &'static str,
    space: RangeInclusive<u32>,
) -> Result<u32, StoreError> {
    if let Some(reason) = HELD.with_borrow(|held| held.get(table).cloned()) {
        return Err(StoreError::NumbersHeld { table, reason });
    }
    SOURCE.with_borrow_mut(|slot| {
        let source = slot.get_or_insert_with(|| Box::new(RandomIds::new()));
        draw(conn, table, &space, source.as_mut())
    })
}

/// `preferred` when `table` doesn't hold it and no reservation covers it; else as [`fresh`]. A new
/// task takes its first crewmate's number this way, so `C-0042` holds `C-0042`.
pub fn fresh_preferring(
    conn: &Connection,
    table: &'static str,
    space: RangeInclusive<u32>,
    preferred: u32,
) -> Result<u32, StoreError> {
    if HELD.with_borrow(|held| !held.contains_key(table))
        && space.contains(&preferred)
        && is_free(conn, table, preferred)?
    {
        return Ok(preferred);
    }
    fresh(conn, table, space)
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
    let extra = if table == "events" {
        " OR EXISTS (SELECT 1 FROM forgotten_parent_facts WHERE id = ?1)"
    } else if table == "messages" {
        " OR EXISTS (SELECT 1 FROM user_message_sources WHERE id = ?1)"
    } else {
        ""
    };
    let held: bool = conn.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE id = ?1){extra}"),
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
    let extra = if table == "events" {
        " UNION SELECT id FROM forgotten_parent_facts WHERE id BETWEEN ?1 AND ?2"
    } else if table == "messages" {
        " UNION SELECT id FROM user_message_sources WHERE id BETWEEN ?1 AND ?2"
    } else {
        ""
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM {table} WHERE id BETWEEN ?1 AND ?2{extra}"
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
