//! The disk mirror: `state.db` keeps the rows [`kept`](crate::kept) names, and the working database
//! is in memory (plan section 3).
//!
//! [`Mirror::open`] migrates `state.db`, copies its kept rows into a fresh in-memory database,
//! prunes the rows no longer kept, nulls the references that dangle, and clears the volatile tables
//! on disk. Writes to a kept table mark it dirty (TEMP triggers on the memory database), and
//! [`Mirror::flush`] then brings `state.db` in line with memory in one transaction. Each kept table
//! is compared with the file row by row, keyed by `seq`, and only the difference is written. A
//! flush never changes memory, so the file is always a copy of memory's kept rows.

use crate::kept::{KEPT, Kept, VOLATILE};
use crate::{StoreError, connection, schema};
use rusqlite::{Connection, Transaction, params_from_iter, types::Value};
use std::collections::BTreeMap;
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
}

impl Mirror {
    /// Migrates `state.db`, loads the kept rows into a new working database, and returns both.
    pub fn open(path: &Path) -> Result<(Connection, Mirror), StoreError> {
        let mut disk = connection::open_disk(path)?;
        schema::migrate(&mut disk)?;
        let mut mem = connection::open_memory()?;
        schema::migrate(&mut mem)?;
        load(&mut mem, path)?;
        disk.execute_batch(&format!("DELETE FROM {}", VOLATILE.join("; DELETE FROM ")))?;
        install_dirty_marks(&mem)?;
        let mut mirror = Mirror {
            disk,
            stale: true,
            erasing: false,
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
            mem.execute("DELETE FROM temp.mirror_dirty", [])?;
            match self.sync(mem) {
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

    /// Writes the difference for every kept table in one transaction. Returns whether any row was
    /// removed or replaced, which leaves old contents in the WAL.
    fn sync(&mut self, mem: &Connection) -> Result<bool, StoreError> {
        let tx = self.disk.transaction()?;
        let mut rewrote = false;
        for kept in KEPT {
            rewrote |= sync_table(&tx, mem, kept)?;
        }
        tx.commit()?;
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
/// name to it. Done after the load, so the load itself marks nothing.
fn install_dirty_marks(mem: &Connection) -> Result<(), StoreError> {
    mem.execute_batch("CREATE TEMP TABLE mirror_dirty (name TEXT PRIMARY KEY) WITHOUT ROWID;")?;
    for kept in KEPT {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            mem.execute_batch(&format!(
                "CREATE TEMP TRIGGER mirror_{t}_{e} AFTER {event} ON {t} BEGIN \
                 INSERT OR IGNORE INTO mirror_dirty (name) VALUES ('{t}'); END;",
                t = kept.table,
                e = event.to_ascii_lowercase(),
            ))?;
        }
    }
    Ok(())
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
    let placeholders = vec!["?"; cols.len()].join(", ");
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
    let mut rewrote = false;
    for (seq, row) in &wanted {
        if stored.get(seq) != Some(row) {
            if stored.contains_key(seq) {
                rewrote = true;
            }
            disk.execute(
                &format!(
                    "INSERT OR REPLACE INTO {t} ({list}) VALUES ({placeholders})",
                    t = kept.table
                ),
                params_from_iter(row.iter()),
            )?;
        }
    }
    for seq in stored.keys().filter(|seq| !wanted.contains_key(*seq)) {
        disk.execute(
            &format!("DELETE FROM {t} WHERE seq = ?1", t = kept.table),
            [seq],
        )?;
        rewrote = true;
    }
    Ok(rewrote)
}

/// Every row of a query whose first column is `seq`, keyed by it.
fn read_rows(conn: &Connection, sql: &str) -> Result<BTreeMap<i64, Vec<Value>>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let width = stmt.column_count();
    let mut rows = BTreeMap::new();
    let mut query = stmt.query([])?;
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
    fn volatile_rows_never_reach_the_file_and_kept_events_do() {
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
            "a message never reaches the file"
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
}
