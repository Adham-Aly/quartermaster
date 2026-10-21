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
}
