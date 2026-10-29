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
        Self::spawn(Store { mem, mirror })
    }

    /// Hands an opened store to a new database thread. The thread seeds the `seq` clock from
    /// the rows it loaded before it takes a job (`seq` is per thread), and `open` waits for that.
    fn spawn(mut store: Store) -> Result<Db, StoreError> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (seeded, ready) = mpsc::channel::<Result<(), StoreError>>();
        std::thread::Builder::new()
            .name("qm-db".into())
            .spawn(move || {
                let result = crate::seq::observe_all(&store.mem);
                let failed = result.is_err();
                let _ = seeded.send(result);
                if failed {
                    return;
                }
                // Ends when every sender (every Db handle) has dropped.
                for job in rx {
                    job(&mut store);
                }
            })
            .expect("the database thread could not be spawned");
        ready
            .recv()
            .unwrap_or_else(|_| Err(StoreError::Panicked("the database thread stopped".into())))?;
        Ok(Db { jobs })
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
