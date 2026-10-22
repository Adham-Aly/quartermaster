//! The single database thread.
//!
//! One thread owns the working database (in memory) and its disk mirror (`mirror.rs`). Callers send
//! it closures over `&mut Connection` and await the result, so nothing else touches SQLite and
//! writes never contend. After each closure the mirror flushes what the closure kept, before the
//! caller hears back: a flush that fails fails the call. The thread stops when the last [`Db`]
//! handle drops; a closure that panics yields [`StoreError::Panicked`] and the thread carries on;
//! a call after the thread has stopped yields [`StoreError::ThreadStopped`].

use crate::StoreError;
use crate::mirror::Mirror;
use rusqlite::Connection;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::mpsc;
use tokio::sync::oneshot;

type Job = Box<dyn FnOnce(&mut Store) + Send + 'static>;

/// The working database and the mirror that keeps `state.db` in step with it.
struct Store {
    mem: Connection,
    mirror: Mirror,
}

/// A cheap, cloneable handle to the database thread.
#[derive(Clone, Debug)]
pub struct Db {
    jobs: mpsc::Sender<Job>,
}

impl Db {
    /// Opens the database at `path`: migrates `state.db`, loads its kept rows into memory, and
    /// spawns the thread. A failing migration rolls back and nothing is applied.
    pub fn open(path: &Path) -> Result<Db, StoreError> {
        let (mem, mirror) = Mirror::open(path)?;
        Ok(Self::spawn(Store { mem, mirror }))
    }

    /// Hands an opened store to a new database thread.
    fn spawn(mut store: Store) -> Db {
        let (jobs, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("qm-db".into())
            .spawn(move || {
                // Ends when every sender (every Db handle) has dropped.
                for job in rx {
                    job(&mut store);
                }
            })
            .expect("the database thread could not be spawned");
        Db { jobs }
    }

    /// Runs `f` on the database thread and returns its result.
    pub async fn call<R, F>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |store: &mut Store| {
            let result = catch_unwind(AssertUnwindSafe(|| f(&mut store.mem)))
                .unwrap_or_else(|panic| Err(StoreError::Panicked(panic_message(&*panic))));
            // Whatever the closure did, the mirror catches up before the caller hears back.
            let flushed = store.mirror.flush(&store.mem);
            let result = match (result, flushed) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(e), _) | (Ok(_), Err(e)) => Err(e),
            };
            // The caller may have stopped waiting; that's fine.
            let _ = tx.send(result);
        });
        self.jobs.send(job).map_err(|_| StoreError::ThreadStopped)?;
        rx.await.map_err(|_| StoreError::ThreadStopped)?
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for one test's `state.db`, removed when the guard drops.
    struct Scratch(std::path::PathBuf);

    /// Two closed tasks, one under the Quartermaster and one under a secondmate, both past
    /// the open-task rule, so their outcomes are the rows the context block reads.
    fn seed_two_orchestrators(conn: &mut Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                 merge_method, created_at, seq) \
             VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
             INSERT INTO agents (id, role, home, status, created_at, seq) \
             VALUES (1, 'quartermaster', '/home', 'idle', 1, 1);
             INSERT INTO secondmates (id, name, scope, created_at) VALUES (1, 'SM', 'p', 1);
             INSERT INTO agents (id, role, home, status, created_at, seq, supervisor_id, secondmate_id) \
             VALUES (2, 'secondmate', '/home', 'idle', 1, 2, 1, 1);",
        )?;
        for (id, supervisor) in [(1, 1), (2, 2)] {
            conn.execute(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, \
                     delivery_mode, branch, status, outcome, created_at, closed_at, seq) \
                 VALUES (?1, 1, ?2, 'build', 't', 'g', 'direct_pr', ?3, 'closed', 'landed', 1, 1, ?1)",
                rusqlite::params![id, supervisor, format!("qm/task-{id}")],
            )?;
        }
        Ok(())
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn a_conversation_never_reaches_the_file_through_the_database() {
        let dir = std::env::temp_dir().join(format!("qm-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let scratch = Scratch(dir.clone());

        let db = Db::open(&dir.join("state.db")).expect("a database");
        db.call(|conn| {
            conn.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                     merge_method, created_at, seq) \
                 VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
                 INSERT INTO agents (id, role, home, status, created_at, seq) \
                 VALUES (1, 'quartermaster', '/home', 'idle', 1, 1);
                 INSERT INTO turns (id, agent_id, started_by, status, started_at, ended_at, seq) \
                 VALUES (1, 1, 'app_event', 'completed', 1, 2, 1);
                 INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) \
                 VALUES (1, 1, 'you', 'message', 'VOLATILE-THROUGH-DB', 1, 1);
                 INSERT INTO session_lines (agent_id, kind, text, created_at, seq) \
                 VALUES (1, 'you', 'VOLATILE-THROUGH-DB', 1, 1);",
            )?;
            Ok(())
        })
        .await
        .expect("the rows");
        db.call(|conn| {
            conn.execute(
                "INSERT INTO events (at, kind, summary, payload, seq) \
                 VALUES (1, 'you.notice', 'kept', '{\"text\":\"KEPT-THROUGH-DB\"}', 2)",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("an event");
        drop(db);

        let mut bytes = std::fs::read(dir.join("state.db")).expect("state.db");
        if let Ok(wal) = std::fs::read(dir.join("state.db-wal")) {
            bytes.extend(wal);
        }
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("VOLATILE-THROUGH-DB"),
            "no conversation is written"
        );
        assert!(text.contains("KEPT-THROUGH-DB"), "a kept event is written");
        drop(scratch);
    }

    #[tokio::test]
    async fn no_table_keeps_a_counter_so_a_restart_forgets_nothing_it_numbers() {
        // IDs are drawn at random and `seq` gives the order, so nothing counts up in the store.
        let dir = std::env::temp_dir().join(format!("qm-counter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let scratch = Scratch(dir.clone());
        let path = dir.join("state.db");

        let db = Db::open(&path).expect("a database");
        db.call(seed_two_orchestrators).await.expect("the rows");
        drop(db);

        let db = Db::open(&path).expect("a restart");
        let counters: i64 = db
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sqlite_sequence'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .expect("the schema");
        assert_eq!(counters, 0, "no AUTOINCREMENT counter exists");
        drop(db);
        drop(scratch);
    }

    #[tokio::test]
    async fn each_orchestrators_newest_five_outcomes_survive_a_restart() {
        use jiff::Timestamp;
        use qm_types::ids::{AgentId, TaskId};
        let dir = std::env::temp_dir().join(format!("qm-outcomes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let scratch = Scratch(dir.clone());
        let path = dir.join("state.db");
        let quartermaster = AgentId::new(1);
        let secondmate = AgentId::new(2);

        let db = Db::open(&path).expect("a database");
        db.call(seed_two_orchestrators).await.expect("the rows");

        // Seven outcomes for the Quartermaster's task and three for the secondmate's.
        db.call(|conn| {
            for i in 0..10_i64 {
                let task: u32 = if i < 7 { 1 } else { 2 };
                crate::tasks::event(
                    conn,
                    "outcome.landed",
                    TaskId::new(task).expect("a task number"),
                    None,
                    &format!("OUT-{i}"),
                    serde_json::json!({}),
                    Timestamp::from_millisecond(1_000 + i).expect("a time"),
                )?;
            }
            Ok(())
        })
        .await
        .expect("the outcomes");

        let before = db
            .call(move |conn| {
                Ok((
                    crate::events::latest_of(conn, "outcome.", quartermaster, 5)?,
                    crate::events::latest_of(conn, "outcome.", secondmate, 5)?,
                ))
            })
            .await
            .expect("the outcomes read");
        let summaries = |rows: &[crate::events::EventRow]| -> Vec<String> {
            rows.iter().map(|r| r.summary.clone()).collect()
        };
        assert_eq!(
            summaries(&before.0),
            ["OUT-6", "OUT-5", "OUT-4", "OUT-3", "OUT-2"],
            "the Quartermaster's five newest outcomes"
        );
        assert_eq!(before.1.len(), 3, "the secondmate's three outcomes");
        assert_eq!(
            before.0[0]
                .payload
                .get("supervisor")
                .and_then(serde_json::Value::as_i64),
            Some(1),
            "a task event names its supervisor"
        );
        // The pump has read every event, so no task is waiting on one (kept.rs).
        db.call(|conn| crate::parent_channel::set_cursor(conn, i64::MAX))
            .await
            .expect("the pump's cursor");
        drop(db);

        let db = Db::open(&path).expect("a restart");
        let kept = db
            .call(move |conn| {
                let total: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE kind LIKE 'outcome.%'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((
                    total,
                    crate::events::latest_of(conn, "outcome.", quartermaster, 5)?,
                    crate::events::latest_of(conn, "outcome.", secondmate, 5)?,
                ))
            })
            .await
            .expect("the outcomes after a restart");
        assert_eq!(
            kept.0, 8,
            "five of the Quartermaster's and all three of the secondmate's are kept"
        );
        assert_eq!(
            summaries(&kept.1),
            summaries(&before.0),
            "the same five after a restart"
        );
        assert_eq!(
            summaries(&kept.2),
            summaries(&before.1),
            "the same three after a restart"
        );
        drop(db);
        drop(scratch);
    }
}
