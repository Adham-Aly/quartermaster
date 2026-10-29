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
