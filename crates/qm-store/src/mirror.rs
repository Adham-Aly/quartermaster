//! The disk mirror: `state.db` keeps the rows [`kept`](crate::kept) names, and the working database
//! is in memory (plan section 3).
//!
//! [`Mirror::open`] migrates `state.db`, copies its kept rows into a fresh in-memory database,
//! prunes the rows no longer kept, nulls the references that dangle, and clears the volatile tables
//! on disk. Writes to a kept table mark it dirty (TEMP triggers on the memory database), and
//! [`Mirror::flush`] then brings `state.db` in line with memory in one transaction. Each kept table
//! is compared with the file row by row, keyed by `seq`, and only the difference is written.
//! A flush never changes memory, so the file is always a copy of memory's kept rows.
//!
//! The chat tables ([`kept::CHAT`]) grow with every streamed line, so comparing them whole on each
//! flush would slow every database call as a chat gets longer. Their triggers mark the changed rows'
//! `seq` instead, and a flush compares only those rows, plus every row of an agent whose chat
//! started or stopped being kept (its engine, `chat_kept_from` or retirement changed), which the
//! mirror notices by comparing each live in-house agent's `chat_kept_from` with the last flush. A
//! stale mirror (after a failed flush) still compares every table whole.

use crate::kept::{self, KEPT, Kept, VOLATILE};
use crate::{StoreError, connection, schema};
use rusqlite::{Connection, Statement, Transaction, params_from_iter, types::Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// The disk side of the store. Owned by the database thread, next to the working connection.
#[derive(Debug)]
pub struct Mirror {
    disk: Connection,
    /// Set when a flush failed: the next flush rewrites every kept table, not only the dirty ones.
    stale: bool,
    /// Set when a rewrite left old contents in the WAL and the checkpoint that erases them was
    /// busy: the next flush tries again, even with nothing new to write.
    erasing: bool,
    /// Each agent whose chat the file keeps, with its `chat_kept_from`, as of the last flush.
    chat_agents: ChatAgents,
}

/// Agent ID to `chat_kept_from`, for the agents whose chat is kept.
type ChatAgents = BTreeMap<i64, i64>;

/// Rows keyed by their `seq`, every column's value in `columns` order.
type Rows = BTreeMap<i64, Vec<Value>>;

impl Mirror {
    /// Migrates `state.db`, loads the kept rows into a new working database, and returns both.
    pub fn open(path: &Path) -> Result<(Connection, Mirror), StoreError> {
        let mut disk = connection::open_disk(path)?;
        schema::migrate_mirror(&mut disk)?;
        let mut mem = connection::open_memory()?;
        schema::migrate(&mut mem)?;
        load(&mut mem, path)?;
        for table in VOLATILE {
            disk.execute(&format!("DELETE FROM {table}"), [])?;
        }
        install_dirty_marks(&mem)?;
        let mut mirror = Mirror {
            disk,
            stale: true,
            erasing: false,
            chat_agents: ChatAgents::new(),
        };
        mirror.flush(&mem)?;
        mirror.disk.execute_batch("VACUUM")?;
        mirror.checkpoint()?;
        Ok((mem, mirror))
    }

    /// Brings `state.db` in line with memory. Does nothing when no kept table changed since the last
    /// flush. On failure the next flush rewrites everything.
    pub fn flush(&mut self, mem: &Connection) -> Result<(), StoreError> {
        let dirty: i64 =
            mem.query_row("SELECT COUNT(*) FROM temp.mirror_dirty", [], |r| r.get(0))?;
        if dirty == 0 && !self.stale && !self.erasing {
            return Ok(());
        }
        if dirty > 0 || self.stale {
            let result = self.sync(mem);
            mem.execute_batch(
                "DELETE FROM temp.mirror_dirty; DELETE FROM temp.mirror_dirty_rows;",
            )?;
            match result {
                Ok(rewrote) => {
                    self.stale = false;
                    self.erasing |= rewrote;
                }
                Err(e) => {
                    self.stale = true;
                    return Err(e);
                }
            }
        }
        if self.erasing {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Writes the difference in one transaction: every kept table, but of the chat tables (unless
    /// the mirror is stale) only the dirty rows and the rows of agents whose chat started or
    /// stopped being kept. Returns whether any row was removed or replaced, which
    /// leaves old contents in the WAL.
    fn sync(&mut self, mem: &Connection) -> Result<bool, StoreError> {
        let dirty: Vec<String> = mem
            .prepare("SELECT name FROM temp.mirror_dirty")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let full = self.stale;
        let chat_agents = if full || dirty.iter().any(|t| t == "agents") {
            Some(chat_agents(mem)?)
        } else {
            None
        };
        let tx = self.disk.transaction()?;
        let mut rewrote = false;
        for kept in KEPT {
            // A predicate may read other tables, so every other table is compared whole.
            if full || !kept::CHAT.contains(&kept.table) {
                rewrote |= sync_table(&tx, mem, kept)?;
                continue;
            }
            if let Some(now) = &chat_agents {
                for agent in changed_agents(&self.chat_agents, now) {
                    rewrote |= sync_agent_rows(&tx, mem, kept, agent)?;
                }
            }
            if dirty.iter().any(|t| t == kept.table) {
                rewrote |= sync_dirty_rows(&tx, mem, kept)?;
            }
        }
        tx.commit()?;
        if let Some(now) = chat_agents {
            self.chat_agents = now;
        }
        Ok(rewrote)
    }

    /// Folds the WAL back into the file and empties it, so the old contents are gone from disk.
    /// The pragma reports whether it could: a reader in the way leaves it busy, and the erasure
    /// stays pending until a checkpoint completes.
    fn checkpoint(&mut self) -> Result<(), StoreError> {
        let busy: i64 = self
            .disk
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        self.erasing = busy != 0;
        Ok(())
    }
}

/// Copies the kept rows of `state.db` into `mem`, prunes what isn't kept, and fixes the references
/// the prune left dangling. Runs with the file attached only for the copy.
fn load(mem: &mut Connection, path: &Path) -> Result<(), StoreError> {
    let text = path.to_str().ok_or_else(|| StoreError::Invalid {
        column: "state.db",
        message: "the data directory's path is not valid UTF-8".to_owned(),
    })?;
    mem.pragma_update(None, "foreign_keys", false)?;
    mem.execute("ATTACH DATABASE ?1 AS disk", [text])?;
    {
        let tx = mem.transaction()?;
        for kept in KEPT {
            let cols = columns(&tx, kept.table)?.join(", ");
            tx.execute(
                &format!(
                    "INSERT INTO main.{t} ({cols}) SELECT {cols} FROM disk.{t} ORDER BY seq",
                    t = kept.table
                ),
                [],
            )?;
        }
        tx.commit()?;
    }
    mem.execute_batch("DETACH DATABASE disk")?;

    {
        let tx = mem.transaction()?;
        for kept in KEPT {
            tx.execute(
                &format!(
                    "DELETE FROM main.{t} WHERE NOT COALESCE(({p}), 0)",
                    t = kept.table,
                    p = kept.predicate
                ),
                [],
            )?;
        }
        // A consumed wake keeps the turn that delivered it (its CHECK ties that to `delivered_at`);
        // an unconsumed one is delivered again, so it forgets the delivery.
        tx.execute(
            "UPDATE main.wakes SET delivered_turn_id = NULL, delivered_at = NULL \
             WHERE consumed_at IS NULL AND delivered_at IS NOT NULL",
            [],
        )?;
        // A turn you started must name your message (a CHECK), so one whose message wasn't kept
        // can't have the reference nulled: it goes. That happens when the message was written
        // under a Claude session and the turn that took it after the switch to the provider engine.
        tx.execute(
            "DELETE FROM main.turns WHERE message_id IS NOT NULL \
             AND message_id NOT IN (SELECT id FROM main.messages)",
            [],
        )?;
        tx.commit()?;
    }
    null_dangling(mem)?;
    mem.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Nulls every reference that names a row the load didn't keep. A dangling reference that isn't
/// nullable means a predicate keeps a row without what it names, which is a bug, so it fails.
fn null_dangling(mem: &mut Connection) -> Result<(), StoreError> {
    with_guards_lifted(mem, |tx| {
        let violations: Vec<(String, Option<i64>, i64)> = tx
            .prepare("PRAGMA foreign_key_check")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (table, rowid, fk) in violations {
            let (column, not_null): (String, i64) = tx.query_row(
                "SELECT f.\"from\", c.\"notnull\" FROM pragma_foreign_key_list(?1) f \
                 JOIN pragma_table_info(?1) c ON c.name = f.\"from\" WHERE f.id = ?2",
                (&table, fk),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let rowid = rowid.ok_or_else(|| StoreError::Invalid {
                column: "state.db",
                message: format!("{table} holds a dangling reference and has no rowid to clear it"),
            })?;
            if not_null == 1 {
                return Err(StoreError::Invalid {
                    column: "state.db",
                    message: format!("{table}.{column} names a row the load didn't keep"),
                });
            }
            tx.execute(
                &format!("UPDATE main.{table} SET {column} = NULL WHERE rowid = ?1"),
                [rowid],
            )?;
        }
        Ok(())
    })
}

/// Runs `repair` in one transaction with every trigger lifted. The triggers guard the app's writes
/// (events and session lines are append-only, a retry's `retry_of` is fixed), and a repair is the
/// one kind of write they must allow: nulling a reference that a deleted row leaves behind. The
/// triggers are put back, exactly as they were, before the commit. The TEMP dirty marks are not
/// main-schema triggers and stay, so the repair still reaches the file.
pub(crate) fn with_guards_lifted<T>(
    conn: &mut Connection,
    repair: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let tx = conn.transaction()?;
    let triggers: Vec<(String, String)> = tx
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, _) in &triggers {
        tx.execute(&format!("DROP TRIGGER \"{name}\""), [])?;
    }
    let value = repair(&tx)?;
    for (_, sql) in &triggers {
        tx.execute(sql, [])?;
    }
    tx.commit()?;
    Ok(value)
}

/// Creates the dirty-table set and a trigger per kept table and write kind that adds the table's
/// name to it; a chat table's triggers also add the changed rows' `seq` (old and new) to the
/// dirty-row set. Done after the load, so the load itself marks nothing.
fn install_dirty_marks(mem: &Connection) -> Result<(), StoreError> {
    mem.execute_batch(
        "CREATE TEMP TABLE mirror_dirty (name TEXT PRIMARY KEY) WITHOUT ROWID;
         CREATE TEMP TABLE mirror_dirty_rows (name TEXT NOT NULL, seq INTEGER NOT NULL, \
             PRIMARY KEY (name, seq)) WITHOUT ROWID;",
    )?;
    for kept in KEPT {
        let chat = kept::CHAT.contains(&kept.table);
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let seqs: &[&str] = match event {
                _ if !chat => &[],
                "INSERT" => &["NEW"],
                "UPDATE" => &["OLD", "NEW"],
                _ => &["OLD"],
            };
            let mut rows = String::new();
            for row in seqs {
                let _ = write!(
                    rows,
                    "INSERT OR IGNORE INTO mirror_dirty_rows (name, seq) \
                     VALUES ('{t}', {row}.seq); ",
                    t = kept.table
                );
            }
            mem.execute_batch(&format!(
                "CREATE TEMP TRIGGER mirror_{t}_{e} AFTER {event} ON {t} BEGIN \
                 INSERT OR IGNORE INTO mirror_dirty (name) VALUES ('{t}'); {rows}END;",
                t = kept.table,
                e = event.to_ascii_lowercase(),
            ))?;
        }
    }
    Ok(())
}

/// The agents whose chat is kept now, with their `chat_kept_from`: the agent side of
/// `current_chat!` in `kept.rs`.
fn chat_agents(mem: &Connection) -> Result<ChatAgents, StoreError> {
    Ok(mem
        .prepare(
            "SELECT id, chat_kept_from FROM main.agents \
             WHERE retired_at IS NULL AND engine = 'provider' AND chat_kept_from IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The agents whose kept chat started, stopped or moved its start between `then` and `now`.
fn changed_agents(then: &ChatAgents, now: &ChatAgents) -> Vec<i64> {
    let mut agents: Vec<i64> = then
        .iter()
        .filter(|(agent, from)| now.get(agent) != Some(from))
        .map(|(agent, _)| *agent)
        .chain(
            now.keys()
                .filter(|agent| !then.contains_key(agent))
                .copied(),
        )
        .collect();
    agents.sort_unstable();
    agents.dedup();
    agents
}

/// The column names of `table`, with `seq` first: the key every kept row is matched by.
fn columns(conn: &Connection, table: &str) -> Result<Vec<String>, StoreError> {
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?
        .query_map([table], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let at = names
        .iter()
        .position(|n| n == "seq")
        .ok_or_else(|| StoreError::Invalid {
            column: "seq",
            message: format!("{table} has no seq column"),
        })?;
    let seq = names.remove(at);
    names.insert(0, seq);
    Ok(names)
}

/// Writes the difference for one kept table: rows the file lacks or holds differently are written,
/// rows the file holds that memory no longer keeps are removed. Returns whether it removed or
/// replaced any row.
fn sync_table(disk: &Connection, mem: &Connection, kept: &Kept) -> Result<bool, StoreError> {
    let cols = columns(mem, kept.table)?;
    let list = cols.join(", ");
    let wanted = read_rows(
        mem,
        &format!(
            "SELECT {list} FROM main.{t} WHERE COALESCE(({p}), 0)",
            t = kept.table,
            p = kept.predicate
        ),
    )?;
    let stored = read_rows(
        disk,
        &format!("SELECT {list} FROM main.{t}", t = kept.table),
    )?;
    write_difference(disk, kept.table, &cols, &wanted, &stored)
}

/// Makes the file's `stored` rows of `table` equal `wanted`: writes the rows it lacks or holds
/// differently and removes the rows memory no longer keeps. Returns whether it removed or replaced
/// any row.
fn write_difference(
    disk: &Connection,
    table: &str,
    cols: &[String],
    wanted: &Rows,
    stored: &Rows,
) -> Result<bool, StoreError> {
    let list = cols.join(", ");
    let placeholders = vec!["?"; cols.len()].join(", ");
    let mut rewrote = false;
    for (seq, row) in wanted {
        if stored.get(seq) != Some(row) {
            if stored.contains_key(seq) {
                rewrote = true;
            }
            disk.execute(
                &format!("INSERT OR REPLACE INTO {table} ({list}) VALUES ({placeholders})"),
                params_from_iter(row.iter()),
            )?;
        }
    }
    for seq in stored.keys().filter(|seq| !wanted.contains_key(*seq)) {
        disk.execute(&format!("DELETE FROM {table} WHERE seq = ?1"), [seq])?;
        rewrote = true;
    }
    Ok(rewrote)
}

/// Writes the difference for one agent's rows of a chat table.
fn sync_agent_rows(
    disk: &Connection,
    mem: &Connection,
    kept: &Kept,
    agent: i64,
) -> Result<bool, StoreError> {
    let cols = columns(mem, kept.table)?;
    let list = cols.join(", ");
    let wanted = read_rows_with(
        mem,
        &format!(
            "SELECT {list} FROM main.{t} WHERE agent_id = ?1 AND COALESCE(({p}), 0)",
            t = kept.table,
            p = kept.predicate
        ),
        [agent],
    )?;
    let stored = read_rows_with(
        disk,
        &format!(
            "SELECT {list} FROM main.{t} WHERE agent_id = ?1",
            t = kept.table
        ),
        [agent],
    )?;
    write_difference(disk, kept.table, &cols, &wanted, &stored)
}

/// Writes the difference for the rows of a chat table its triggers marked since the last flush.
fn sync_dirty_rows(disk: &Connection, mem: &Connection, kept: &Kept) -> Result<bool, StoreError> {
    let cols = columns(mem, kept.table)?;
    let list = cols.join(", ");
    let seqs: Vec<i64> = mem
        .prepare("SELECT seq FROM temp.mirror_dirty_rows WHERE name = ?1")?
        .query_map([kept.table], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut wanted_one = mem.prepare(&format!(
        "SELECT {list} FROM main.{t} WHERE seq = ?1 AND COALESCE(({p}), 0)",
        t = kept.table,
        p = kept.predicate
    ))?;
    let mut stored_one = disk.prepare(&format!(
        "SELECT {list} FROM main.{t} WHERE seq = ?1",
        t = kept.table
    ))?;
    let mut wanted = BTreeMap::new();
    let mut stored = BTreeMap::new();
    for seq in seqs {
        wanted.extend(collect_rows(&mut wanted_one, [seq])?);
        stored.extend(collect_rows(&mut stored_one, [seq])?);
    }
    write_difference(disk, kept.table, &cols, &wanted, &stored)
}

/// Every row of a query whose first column is `seq`, keyed by it.
fn read_rows(conn: &Connection, sql: &str) -> Result<Rows, StoreError> {
    read_rows_with(conn, sql, [])
}

/// [`read_rows`] with parameters.
fn read_rows_with(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Rows, StoreError> {
    collect_rows(&mut conn.prepare(sql)?, params)
}

/// The rows of a prepared query whose first column is `seq`, keyed by it.
fn collect_rows(
    stmt: &mut Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Rows, StoreError> {
    let width = stmt.column_count();
    let mut rows = BTreeMap::new();
    let mut query = stmt.query(params)?;
    while let Some(row) = query.next()? {
        let values = (0..width)
            .map(|i| row.get::<_, Value>(i))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Value::Integer(seq) = values[0] else {
            return Err(StoreError::Invalid {
                column: "seq",
                message: "a kept row has no integer seq".to_owned(),
            });
        };
        rows.insert(seq, values);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for one test's `state.db`, removed when the guard drops.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("qm-mirror-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch folder");
            Scratch(dir)
        }

        fn state(&self) -> std::path::PathBuf {
            self.0.join("state.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("a count")
    }

    /// The quartermaster agent, the one row every other test's references name.
    fn quartermaster(mem: &Connection) {
        mem.execute(
            "INSERT INTO agents (id, role, home, status, created_at, seq) \
             VALUES (1, 'quartermaster', '/home', 'idle', 1, 1)",
            [],
        )
        .expect("an agent");
    }

    #[test]
    fn only_kept_rows_reach_the_file_and_load_back() {
        let scratch = Scratch::new("kept-round-trip");
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
            mem.execute_batch(
                "INSERT INTO secondmates (id, name, scope, created_at, retired_at, seq) \
                     VALUES (1, 'ops', 'scope', 1, NULL, 1), (2, 'old', 'scope', 1, 5, 2);
                 INSERT INTO standing_instructions (id, text, created_at, seq) \
                     VALUES (1, 'keep it short', 1, 3);",
            )
            .expect("rows");
            mirror.flush(&mem).expect("a flush");
            let disk = Connection::open(scratch.state()).expect("the file");
            assert_eq!(
                count(&disk, "secondmates"),
                1,
                "a retired secondmate isn't kept"
            );
            assert_eq!(count(&disk, "standing_instructions"), 1);
        }
        let (mem, _mirror) = Mirror::open(&scratch.state()).expect("a reopened mirror");
        assert_eq!(count(&mem, "secondmates"), 1);
        assert_eq!(count(&mem, "standing_instructions"), 1);
    }

    #[test]
    fn a_chat_off_the_provider_engine_never_reaches_the_file_and_kept_events_do() {
        let scratch = Scratch::new("no-history");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        quartermaster(&mem);
        mem.execute_batch(
            "INSERT INTO turns (id, agent_id, started_by, started_at, seq) \
                 VALUES (1, 1, 'app_event', 1, 1);
             INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) \
                 VALUES (1, 1, 'you', 'message', 'VOLATILE-WORDS', 1, 1);
             INSERT INTO events (at, kind, summary, payload, seq) \
                 VALUES (1, 'you.notice', 'kept', '{\"text\":\"KEPT-WORDS\"}', 1);",
        )
        .expect("rows");
        mirror.flush(&mem).expect("a flush");
        mirror.checkpoint().expect("a checkpoint");

        let mut bytes = std::fs::read(scratch.state()).expect("state.db");
        if let Ok(wal) = std::fs::read(scratch.0.join("state.db-wal")) {
            bytes.extend(wal);
        }
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("VOLATILE-WORDS"),
            "a message of an agent off the provider engine never reaches the file"
        );
        assert!(text.contains("KEPT-WORDS"), "a kept event does");
    }

    #[test]
    fn a_dangling_reference_loads_as_empty_and_an_undelivered_wake_is_delivered_again() {
        let scratch = Scratch::new("dangling");
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
            quartermaster(&mem);
            mem.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                     merge_method, created_at, seq) \
                     VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
                 INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, \
                     delivery_mode, branch, created_at, seq) \
                     VALUES (5, 1, 1, 'build', 't', 'g', 'direct_pr', 'qm/task-5', 1, 1);
                 INSERT INTO events (at, kind, summary, payload, turn_id, seq) \
                     VALUES (1, 'you.notice', 'kept', '{}', NULL, 1);
                 INSERT INTO wakes (id, target_agent_id, kind, payload, dedupe_key, task_id, \
                     created_at, delivered_turn_id, delivered_at, consumed_at, seq) \
                     VALUES (1, 1, 'k', '{}', 'a', 5, 1, 7, 2, 3, 2), \
                            (2, 1, 'k', '{}', 'b', NULL, 1, 8, 2, NULL, 3);",
            )
            .expect("rows");
            mirror.flush(&mem).expect("a flush");
            // The file holds an event that names a turn the next load won't have.
            mirror
                .disk
                .execute(
                    "INSERT INTO events (id, at, kind, summary, payload, turn_id, seq) \
                     VALUES (42, 1, 'you.dangling', 'x', '{}', 42, 99)",
                    [],
                )
                .expect("a dangling turn");
        }
        let (mem, _mirror) = Mirror::open(&scratch.state()).expect("a reopened mirror");
        let turn: Option<i64> = mem
            .query_row(
                "SELECT turn_id FROM events WHERE kind = 'you.dangling'",
                [],
                |r| r.get(0),
            )
            .expect("the event");
        assert_eq!(turn, None, "a dangling nullable reference loads as NULL");
        let consumed: Option<i64> = mem
            .query_row(
                "SELECT delivered_turn_id FROM wakes WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .expect("the consumed wake");
        assert_eq!(
            consumed,
            Some(7),
            "a consumed wake keeps the turn that delivered it"
        );
        let pending: (Option<i64>, Option<i64>) = mem
            .query_row(
                "SELECT delivered_turn_id, delivered_at FROM wakes WHERE id = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the undelivered wake");
        assert_eq!(
            pending,
            (None, None),
            "an unconsumed wake is delivered again"
        );
    }

    #[test]
    fn previous_mirrors_upgrade_before_dangling_turn_references_are_checked() {
        // Versions 8 and 9 exercise migrations 0010 and 0011 respectively. Both mirrors are
        // valid disk shapes: their event survives, but its volatile turn does not.
        for version in [8, 9] {
            let scratch = Scratch::new(&format!("incomplete-upgrade-{version}"));
            {
                let mut old = connection::open_disk(&scratch.state()).expect("old mirror");
                rusqlite_migration::Migrations::from_slice(&schema::MIGRATIONS[..version])
                    .to_latest(&mut old)
                    .expect("old schema");
                quartermaster(&old);
                old.execute_batch(
                    "INSERT INTO events (id, at, kind, summary, payload, turn_id, seq) \
                     VALUES (7, 1, 'secondmate.routed', 'mirror retained event', '{}', 999, 7);",
                )
                .expect("incomplete mirror event");
            }
            let (mem, mirror) = Mirror::open(&scratch.state()).expect("upgrade incomplete mirror");
            let turn: Option<i64> = mem
                .query_row("SELECT turn_id FROM events WHERE id = 7", [], |r| r.get(0))
                .expect("retained event");
            assert_eq!(turn, None);
            assert_eq!(count(&mem, "pragma_foreign_key_check"), 0);
            assert!(
                mem.execute("UPDATE events SET summary = 'bad'", [])
                    .is_err()
            );
            assert!(
                mem.execute(
                    "INSERT INTO turns (id, agent_id, started_by, started_at, seq) \
                 VALUES (999, 999, 'app_event', 1, 999)",
                    [],
                )
                .is_err(),
                "working writes still enforce foreign keys"
            );
            drop((mem, mirror));
            Mirror::open(&scratch.state()).expect("second start");
        }
    }

    #[test]
    fn mirror_upgrade_still_rejects_missing_required_working_dependencies() {
        let scratch = Scratch::new("invalid-required-reference");
        {
            let mut old = connection::open_disk(&scratch.state()).expect("old mirror");
            rusqlite_migration::Migrations::from_slice(&schema::MIGRATIONS[..8])
                .to_latest(&mut old)
                .expect("old schema");
            quartermaster(&old);
            old.execute_batch(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, \
                     delivery_mode, status, created_at, seq) \
                 VALUES (3, 999, 1, 'build', 'task', 'goal', 'folder', 'queued', 1, 3);",
            )
            .expect("corrupt mirror missing its required project");
        }
        let error =
            Mirror::open(&scratch.state()).expect_err("required dependency is not repairable");
        assert!(error.to_string().contains("tasks.project_id"), "{error}");
    }

    #[test]
    fn unread_closed_tasks_keep_removed_projects_and_retired_supervisors() {
        let scratch = Scratch::new("closed-task-dependencies");
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("mirror");
            quartermaster(&mem);
            mem.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                     merge_method, created_at, removed_at, seq) \
                     VALUES (2, 'gone', '/gone', 'created', 'main', 'folder', 'merge', 1, 2, 2);
                 INSERT INTO secondmates (id, name, scope, created_at, retired_at, seq) \
                     VALUES (11, 'old', 'scope', 1, 2, 11), (21, 'request owner', 'scope', 1, 2, 21);
                 INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, \
                     created_at, retired_at, seq) \
                     VALUES (12, 'secondmate', 1, 11, '/sm', 'retired', 1, 2, 12);
                 INSERT INTO secondmate_requests (id, secondmate_id, request, status, created_at, seq) \
                     VALUES (13, 21, 'request', 'closed', 1, 13);
                 INSERT INTO tasks (id, project_id, supervisor_id, secondmate_request_id, kind, \
                     title, goal, delivery_mode, status, outcome, created_at, closed_at, seq) \
                     VALUES (3, 2, 12, 13, 'build', 'closed', 'fixture', 'folder', 'closed', \
                             'answered', 1, 2, 3);
                 INSERT INTO events (id, at, kind, summary, payload, task_id, seq) \
                     VALUES (4, 1, 'task.closed', 'unread fact', '{}', 3, 4);
                 INSERT INTO parent_channel_cursor (seq, event_seq) VALUES (1, 0);",
            ).expect("reviewer fixture with retired dependencies");
            mirror
                .flush(&mem)
                .expect("mirror closed task dependency closure");
        }
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("restart with unread task");
        for table in ["projects", "tasks", "secondmate_requests"] {
            assert_eq!(
                count(&mem, table),
                1,
                "{table} survives with the unread task"
            );
        }
        assert_eq!(count(&mem, "agents"), 2);
        assert_eq!(count(&mem, "secondmates"), 2);
        assert_eq!(count(&mem, "pragma_foreign_key_check"), 0);
        mem.execute("UPDATE parent_channel_cursor SET event_seq = 4", [])
            .expect("delivered");
        mirror.flush(&mem).expect("flush delivery");
        drop((mem, mirror));
        let (mem, _) = Mirror::open(&scratch.state()).expect("restart after delivery");
        assert_eq!(count(&mem, "tasks"), 0);
        assert_eq!(count(&mem, "projects"), 0);
        assert_eq!(count(&mem, "secondmates"), 0);
    }

    #[test]
    fn a_busy_checkpoint_keeps_the_erasure_pending_until_one_completes() {
        let scratch = Scratch::new("busy-checkpoint");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        mem.execute(
            "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                 merge_method, created_at, seq) \
             VALUES (2, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 2)",
            [],
        )
        .expect("a project");
        mirror.flush(&mem).expect("the project is written");

        // A reader holding its snapshot keeps the checkpoint from finishing.
        let reader = Connection::open(scratch.state()).expect("a reader");
        reader.execute_batch("BEGIN").expect("a read transaction");
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .expect("the snapshot");
        mem.execute("DELETE FROM projects WHERE id = 2", [])
            .expect("the removal");
        mirror.flush(&mem).expect("the removal is written");
        assert!(
            mirror.erasing,
            "a busy checkpoint leaves the old row in the WAL, so the erasure is pending"
        );

        reader.execute_batch("COMMIT").expect("the reader lets go");
        drop(reader);
        mirror.flush(&mem).expect("the retry needs no new rows");
        assert!(!mirror.erasing, "the checkpoint completed");
    }

    /// Everything `state.db` holds, the WAL included: what a reader of the folder would find.
    fn file_text(scratch: &Scratch) -> String {
        let mut bytes = std::fs::read(scratch.state()).expect("state.db");
        if let Ok(wal) = std::fs::read(scratch.0.join("state.db-wal")) {
            bytes.extend(wal);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn a_failed_flush_is_written_again_by_the_next_one() {
        let scratch = Scratch::new("flush-failure");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        quartermaster(&mem);
        mirror.flush(&mem).expect("a first flush");

        // The disk refuses projects for now; memory has a project it can't write.
        mirror
            .disk
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON projects \
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .expect("a refusing trigger");
        mem.execute(
            "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                 merge_method, created_at, seq) \
             VALUES (2, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 2)",
            [],
        )
        .expect("a project");
        assert!(
            mirror.flush(&mem).is_err(),
            "the refusal reaches the caller"
        );
        assert!(mirror.stale, "the next flush will rewrite every kept table");

        mirror
            .disk
            .execute_batch("DROP TRIGGER refuse")
            .expect("the trigger goes");
        // Nothing changed in memory since the failure, yet the next flush writes the project.
        mirror.flush(&mem).expect("the retry");
        assert!(!mirror.stale);
        assert_eq!(
            count(&mirror.disk, "projects"),
            1,
            "the refused project is written"
        );
    }

    #[test]
    fn an_upgrade_from_0008_keeps_only_kept_rows_and_a_second_start_changes_nothing() {
        let scratch = Scratch::new("upgrade");
        {
            let mut old = connection::open_disk(&scratch.state()).expect("the old file");
            rusqlite_migration::Migrations::from_slice(&schema::MIGRATIONS[..7])
                .to_latest(&mut old)
                .expect("the 0008 schema");
            old.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                     merge_method, created_at) \
                 VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1);
                 INSERT INTO agents (id, role, home, status, created_at) \
                 VALUES (1, 'quartermaster', '/home', 'idle', 1);
                 INSERT INTO turns (id, agent_id, started_by, status, started_at, ended_at) \
                 VALUES (1, 1, 'app_event', 'completed', 1, 2);
                 INSERT INTO messages (id, agent_id, source, kind, text, created_at) \
                 VALUES (1, 1, 'you', 'message', 'VOLATILE-OLD', 1);
                 INSERT INTO session_lines (agent_id, kind, text, created_at) \
                 VALUES (1, 'you', 'VOLATILE-OLD', 1);
                 INSERT INTO events (at, kind, summary, payload) \
                 VALUES (1, 'you.notice', 'kept', '{\"text\":\"KEPT-OLD\"}');",
            )
            .expect("the old rows");
        }

        let (mem, mirror) = Mirror::open(&scratch.state()).expect("the upgrade");
        assert_eq!(count(&mem, "projects"), 1, "a kept row loads");
        assert_eq!(count(&mem, "messages"), 0, "a message never loads");
        let seq: i64 = mem
            .query_row("SELECT seq FROM events WHERE id = 1", [], |r| r.get(0))
            .expect("the event");
        assert_eq!(seq, 1, "the upgrade gives the event its creation order");
        drop((mem, mirror));
        assert!(
            !file_text(&scratch).contains("VOLATILE-OLD"),
            "the upgrade wipes the old conversation from the file"
        );
        assert!(file_text(&scratch).contains("KEPT-OLD"));

        // A start always rebuilds the file with VACUUM, so the bytes can differ; the rows and the
        // schema version must not.
        let stored = |path: &Path| -> String {
            let disk = connection::open_disk(path).expect("state.db");
            let version: i64 = disk
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .expect("a version");
            let events: Vec<(i64, i64)> = disk
                .prepare("SELECT id, seq FROM events ORDER BY id")
                .expect("a query")
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("rows")
                .collect::<rusqlite::Result<_>>()
                .expect("values");
            format!(
                "v{version} projects={} events={events:?}",
                count(&disk, "projects")
            )
        };
        let before = stored(&scratch.state());
        let (mem, mirror) = Mirror::open(&scratch.state()).expect("a second start");
        drop((mem, mirror));
        assert_eq!(
            stored(&scratch.state()),
            before,
            "a second start changes no row"
        );
    }

    /// Two secondmates' agents beside an in-house Quartermaster: agent 1 on the provider engine,
    /// agent 2 on Claude, agent 3 on the provider engine; each provider agent's chat kept from its
    /// own `seq`.
    fn crew(mem: &Connection) {
        mem.execute_batch(
            "INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq) \
                 VALUES (1, 'claude', 'claude', 'subscription', 'claude', 1, 1), \
                        (2, 'custom', 'stub', 'api_key', 'provider', 1, 2);
             INSERT INTO secondmates (id, name, scope, created_at, seq) \
                 VALUES (1, 'ops', 'scope', 1, 1), (2, 'research', 'scope', 1, 2);
             INSERT INTO agents (id, role, supervisor_id, secondmate_id, engine, home, status, \
                 created_at, seq, chat_kept_from) \
                 VALUES (1, 'quartermaster', NULL, NULL, 'provider', '/h', 'idle', 1, 1, 1), \
                        (2, 'secondmate', 1, 1, 'claude', '/sm', 'idle', 1, 2, NULL), \
                        (3, 'secondmate', 1, 2, 'provider', '/sm2', 'idle', 1, 3, 3);",
        )
        .expect("the crew");
        // Rows made from now on sort after the crew, as they would in the app.
        crate::seq::observe(100);
    }

    /// One exchange of `agent`'s chat: a message, an app turn and a session line with `text`, each
    /// with the next `seq`. Returns the message's ID; the turn and the line share it.
    fn chat(mem: &Connection, agent: i64, text: &str) -> i64 {
        let id = crate::seq::next_millis(0);
        mem.execute(
            "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) \
             VALUES (?1, ?2, 'agent', 'reply', ?3, 1, ?1)",
            rusqlite::params![id, agent, text],
        )
        .expect("a message");
        let seq = crate::seq::next_millis(0);
        mem.execute(
            "INSERT INTO turns (id, agent_id, started_by, status, started_at, ended_at, seq) \
             VALUES (?1, ?2, 'app_event', 'completed', 1, 2, ?3)",
            rusqlite::params![id, agent, seq],
        )
        .expect("a turn");
        let seq = crate::seq::next_millis(0);
        mem.execute(
            "INSERT INTO session_lines (id, agent_id, kind, text, message_id, created_at, seq) \
             VALUES (?1, ?2, 'agent', ?3, ?1, 1, ?4)",
            rusqlite::params![id, agent, text, seq],
        )
        .expect("a session line");
        id
    }

    /// `agent`'s rows in each chat table of `conn`, as `table:count` words.
    fn chat_rows(conn: &Connection, agent: i64) -> String {
        kept::CHAT
            .iter()
            .map(|t| {
                let n: i64 = conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {t} WHERE agent_id = ?1"),
                        [agent],
                        |r| r.get(0),
                    )
                    .expect("a count");
                format!("{t}:{n}")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn an_in_house_chat_survives_a_restart_and_a_claude_chat_never_reaches_the_file() {
        let scratch = Scratch::new("chat-restart");
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
            crew(&mem);
            chat(&mem, 1, "IN-HOUSE-WORDS");
            chat(&mem, 2, "CLAUDE-WORDS");
            mirror.flush(&mem).expect("a flush");
            mirror.checkpoint().expect("a checkpoint");
            let text = file_text(&scratch);
            assert!(text.contains("IN-HOUSE-WORDS"), "an in-house chat is kept");
            assert!(
                !text.contains("CLAUDE-WORDS"),
                "a Claude agent's chat never reaches the file"
            );
        }
        let (mem, _mirror) = Mirror::open(&scratch.state()).expect("a restart");
        assert_eq!(
            chat_rows(&mem, 1),
            "messages:1 turns:1 session_lines:1",
            "the in-house chat loads back whole"
        );
        assert_eq!(chat_rows(&mem, 2), "messages:0 turns:0 session_lines:0");
        let line_message: Option<i64> = mem
            .query_row("SELECT message_id FROM session_lines", [], |r| r.get(0))
            .expect("the line");
        assert!(line_message.is_some(), "the line still names its message");
    }

    #[test]
    fn a_compacted_or_cleared_chat_leaves_only_the_current_rows_in_the_file() {
        let scratch = Scratch::new("chat-forget");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        crew(&mem);
        let old = chat(&mem, 1, "BEFORE-COMPACTION");
        chat(&mem, 1, "AFTER-COMPACTION");
        mirror.flush(&mem).expect("a flush");
        // What compaction does to memory: the rows before the boundary go.
        let mut mem = mem;
        with_guards_lifted(&mut mem, |tx| {
            for (table, column) in [
                ("session_lines", "message_id"),
                ("turns", "id"),
                ("messages", "id"),
            ] {
                tx.execute(&format!("DELETE FROM {table} WHERE {column} = ?1"), [old])?;
            }
            Ok(())
        })
        .expect("the compaction");
        mirror.flush(&mem).expect("a flush");
        assert!(mirror.erasing || !file_text(&scratch).contains("BEFORE-COMPACTION"));
        mirror.checkpoint().expect("a checkpoint");
        let text = file_text(&scratch);
        assert!(
            !text.contains("BEFORE-COMPACTION"),
            "no copy of the dropped history stays on disk"
        );
        assert!(text.contains("AFTER-COMPACTION"));
        assert_eq!(
            chat_rows(&mirror.disk, 1),
            "messages:1 turns:1 session_lines:1"
        );
    }

    #[test]
    fn a_retired_agents_chat_leaves_the_file_with_its_next_flush() {
        let scratch = Scratch::new("chat-retire");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        crew(&mem);
        chat(&mem, 3, "RETIRING-WORDS");
        mirror.flush(&mem).expect("a flush");
        assert_eq!(
            chat_rows(&mirror.disk, 3),
            "messages:1 turns:1 session_lines:1"
        );
        crate::secondmates::retire(
            &mem,
            qm_types::ids::SecondmateId::new(2).expect("an ID"),
            jiff::Timestamp::UNIX_EPOCH,
        )
        .expect("the retirement");
        mirror.flush(&mem).expect("a flush");
        mirror.checkpoint().expect("a checkpoint");
        assert_eq!(
            chat_rows(&mirror.disk, 3),
            "messages:0 turns:0 session_lines:0",
            "memory still has the rows; the file doesn't"
        );
        assert!(!file_text(&scratch).contains("RETIRING-WORDS"));
        assert_eq!(chat_rows(&mem, 3), "messages:1 turns:1 session_lines:1");
    }

    #[test]
    fn only_rows_made_on_the_provider_engine_reach_the_file() {
        use qm_types::domain::{Effort, EngineKind};
        use qm_types::ids::{AccountId, AgentId, ModelId};
        let scratch = Scratch::new("chat-switch");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        crew(&mem);
        let model = ModelId::new("stub-model").expect("a model");
        let kept_from = |agent: i64| -> Option<i64> {
            mem.query_row(
                "SELECT chat_kept_from FROM agents WHERE id = ?1",
                [agent],
                |r| r.get(0),
            )
            .expect("the agent")
        };
        let route = |agent: i64, engine: EngineKind, account: i64| {
            crate::agents::set_routed(
                &mem,
                AgentId::new(agent),
                engine,
                AccountId::new(account),
                &model,
                Effort::Medium,
            )
            .expect("the route");
        };

        chat(&mem, 2, "WHILE-ON-CLAUDE");
        route(2, EngineKind::Provider, 2);
        let from = kept_from(2).expect("a provider agent's chat is kept");
        assert!(from > 2, "it starts after the rows made under Claude");
        chat(&mem, 2, "AFTER-THE-SWITCH");
        mirror.flush(&mem).expect("a flush");
        mirror.checkpoint().expect("a checkpoint");
        let text = file_text(&scratch);
        assert!(!text.contains("WHILE-ON-CLAUDE"));
        assert!(text.contains("AFTER-THE-SWITCH"));

        route(2, EngineKind::Provider, 2);
        assert_eq!(
            kept_from(2),
            Some(from),
            "a new model on the same engine keeps the start"
        );

        route(2, EngineKind::Claude, 1);
        assert_eq!(kept_from(2), None);
        chat(&mem, 2, "BACK-ON-CLAUDE");
        mirror.flush(&mem).expect("a flush");
        mirror.checkpoint().expect("a checkpoint");
        let text = file_text(&scratch);
        assert!(
            !text.contains("AFTER-THE-SWITCH") && !text.contains("BACK-ON-CLAUDE"),
            "a switch to Claude takes the chat off the file"
        );
        assert_eq!(
            chat_rows(&mirror.disk, 2),
            "messages:0 turns:0 session_lines:0"
        );

        // An agent that never had an engine keeps its chat from its own start.
        mem.execute(
            "INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, \
                 created_at, seq) VALUES (4, 'secondmate', 1, 2, '/sm4', 'starting', 1, 4)",
            [],
        )
        .expect("a new agent");
        route(4, EngineKind::Provider, 2);
        assert_eq!(kept_from(4), Some(4));
    }

    #[test]
    fn a_turn_you_started_whose_message_wasnt_kept_does_not_load() {
        let scratch = Scratch::new("chat-orphan-turn");
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
            crew(&mem);
            chat(&mem, 1, "KEPT");
            mirror.flush(&mem).expect("a flush");
            // The message was made under Claude and never reached the file; its turn did.
            mirror
                .disk
                .execute_batch(
                    "INSERT INTO turns (id, agent_id, started_by, message_id, status, \
                         started_at, ended_at, seq) \
                     VALUES (77, 1, 'user', 76, 'completed', 1, 2, 9000000000000000);
                     INSERT INTO session_lines (id, agent_id, kind, text, message_id, turn_id, \
                         created_at, seq) \
                     VALUES (78, 1, 'you', 'hi', 76, 77, 1, 9000000000000001);",
                )
                .expect("rows that name a missing message");
        }
        let (mem, _mirror) = Mirror::open(&scratch.state()).expect("a restart");
        assert_eq!(count(&mem, "pragma_foreign_key_check"), 0);
        let turn: Option<i64> = mem
            .query_row("SELECT turn_id FROM session_lines WHERE id = 78", [], |r| {
                r.get(0)
            })
            .expect("the line stays");
        assert_eq!(turn, None, "the line forgets the turn that went");
        assert_eq!(chat_rows(&mem, 1), "messages:1 turns:1 session_lines:2");
    }

    /// The file's chat rows are exactly memory's kept ones, however the chat changed: the row-by-row
    /// flush agrees with comparing every table whole.
    #[test]
    fn the_row_by_row_flush_matches_a_whole_comparison() {
        use qm_types::domain::{Effort, EngineKind};
        use qm_types::ids::{AccountId, AgentId, ModelId};
        let scratch = Scratch::new("chat-equivalence");
        let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
        crew(&mem);
        let mut mem = mem;
        let model = ModelId::new("stub-model").expect("a model");
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |n: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % n
        };
        for step in 0..300 {
            let agent = i64::try_from(next(3)).expect("small") + 1;
            match next(10) {
                0..=4 => {
                    chat(&mem, agent, &format!("step {step}"));
                }
                5 => {
                    mem.execute(
                        "UPDATE turns SET effective_actions = json_array(?2) \
                         WHERE agent_id = ?1 AND id = (SELECT MAX(id) FROM turns WHERE agent_id = ?1)",
                        rusqlite::params![agent, step],
                    )
                    .expect("a turn update");
                }
                6 => {
                    // Compaction or clear: everything but the newest exchange goes.
                    with_guards_lifted(&mut mem, |tx| {
                        let newest: Option<i64> = tx.query_row(
                            "SELECT MAX(id) FROM messages WHERE agent_id = ?1",
                            [agent],
                            |r| r.get(0),
                        )?;
                        let newest = newest.unwrap_or(0);
                        tx.execute(
                            "DELETE FROM session_lines WHERE agent_id = ?1 AND message_id < ?2",
                            [agent, newest],
                        )?;
                        tx.execute(
                            "DELETE FROM turns WHERE agent_id = ?1 AND id < ?2",
                            [agent, newest],
                        )?;
                        tx.execute(
                            "DELETE FROM messages WHERE agent_id = ?1 AND id < ?2",
                            [agent, newest],
                        )?;
                        Ok(())
                    })
                    .expect("a forget");
                }
                7 if agent != 1 => {
                    let (engine, account) = if next(2) == 0 {
                        (EngineKind::Claude, 1)
                    } else {
                        (EngineKind::Provider, 2)
                    };
                    crate::agents::set_routed(
                        &mem,
                        AgentId::new(agent),
                        engine,
                        AccountId::new(account),
                        &model,
                        Effort::Medium,
                    )
                    .expect("a route");
                }
                _ => {}
            }
            if next(3) == 0 {
                continue; // several changes reach one flush
            }
            mirror.flush(&mem).expect("a flush");
            for kept in KEPT.iter().filter(|k| kept::CHAT.contains(&k.table)) {
                let list = columns(&mem, kept.table).expect("columns").join(", ");
                let wanted = read_rows(
                    &mem,
                    &format!(
                        "SELECT {list} FROM main.{t} WHERE COALESCE(({p}), 0)",
                        t = kept.table,
                        p = kept.predicate
                    ),
                )
                .expect("memory");
                let stored = read_rows(&mirror.disk, &format!("SELECT {list} FROM {}", kept.table))
                    .expect("the file");
                assert_eq!(stored, wanted, "{} after step {step}", kept.table);
            }
        }
    }

    /// A kill mid-turn leaves the in-house seat's running turn in the file; startup interrupts it,
    /// the seat finds it cut short, and a new turn can start.
    #[test]
    fn a_turn_running_at_a_kill_is_interrupted_at_the_next_start() {
        let scratch = Scratch::new("chat-kill");
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
            crew(&mem);
            let message = chat(&mem, 1, "YOUR-QUESTION");
            let seq = crate::seq::next_millis(0);
            mem.execute(
                "INSERT INTO turns (id, agent_id, started_by, message_id, started_at, seq) \
                 VALUES (5, 1, 'user', ?1, 1, ?2)",
                [message, seq],
            )
            .expect("a running turn");
            mirror.flush(&mem).expect("a flush");
            // No shutdown: the process is killed here.
        }
        let (mut mem, _mirror) = Mirror::open(&scratch.state()).expect("a restart");
        let at = jiff::Timestamp::UNIX_EPOCH;
        let interrupted =
            crate::agents::interrupt_running_turns(&mut mem, at).expect("the startup interrupt");
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].task, None, "the seat's turn has no task");
        let cut = crate::turns::cut_short(&mem, qm_types::ids::AgentId::new(1)).expect("cut");
        assert_eq!(cut.len(), 1, "the seat's recovery finds the turn cut short");
        mem.execute(
            "INSERT INTO turns (id, agent_id, started_by, started_at, seq) \
             VALUES (6, 1, 'app_event', 2, ?1)",
            [crate::seq::next_millis(0)],
        )
        .expect("a new turn starts");
    }

    #[test]
    fn a_notice_under_a_kept_message_comes_back_with_its_answer() {
        let scratch = Scratch::new("chat-notice");
        let event = |mem: &Connection, message: i64, kind: &str, payload: &str| {
            let seq = crate::seq::next_millis(0);
            mem.execute(
                "INSERT INTO events (id, at, kind, summary, payload, seq) \
                 VALUES (?1, 1, ?2, 'notice', json_set(?3, '$.message', ?4), ?1)",
                rusqlite::params![seq, kind, payload, message],
            )
            .expect("an event");
        };
        // Whether each message's notice was answered; a message without one is left out.
        let answered = |conn: &Connection, messages: &[i64]| -> Vec<bool> {
            let ids: Vec<_> = messages
                .iter()
                .copied()
                .map(qm_types::ids::MessageId::new)
                .collect();
            crate::events::notices(conn, &ids)
                .expect("the notices")
                .into_values()
                .map(|n| n.answer.is_some())
                .collect()
        };
        {
            let (mem, mut mirror) = Mirror::open(&scratch.state()).expect("a new mirror");
            crew(&mem);
            // Your messages 1 and 2 to the in-house seat, 3 to the Claude secondmate.
            for (message, agent) in [(1, 1), (2, 1), (3, 2)] {
                mem.execute(
                    "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) \
                     VALUES (?1, ?2, 'you', 'message', 'words', 1, ?3)",
                    rusqlite::params![message, agent, crate::seq::next_millis(0)],
                )
                .expect("your message");
                event(
                    &mem,
                    message,
                    crate::events::NOTICE,
                    r#"{"cause":"failed"}"#,
                );
            }
            event(
                &mem,
                2,
                crate::events::NOTICE_ANSWERED,
                r#"{"answer":"leave_it"}"#,
            );
            mirror.flush(&mem).expect("a flush");
            assert_eq!(answered(&mem, &[1, 2, 3]), [false, true, false]);
        }
        let (mem, _mirror) = Mirror::open(&scratch.state()).expect("a restart");
        assert_eq!(
            answered(&mem, &[1, 2, 3]),
            [false, true],
            "the in-house notices come back, the answered one still answered; the Claude \
             agent's goes with its chat"
        );
    }
}
