//! The single database thread.
//!
//! One thread owns the one connection. Callers send it closures over `&mut Connection` and await
//! the result, so nothing else touches SQLite and writes never contend. The thread stops when the
//! last [`Db`] handle drops; a closure that panics yields [`StoreError::Panicked`] and the thread
//! carries on; a call after the thread has stopped yields [`StoreError::ThreadStopped`].

use crate::{StoreError, connection, schema};
use rusqlite::Connection;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::mpsc;
use tokio::sync::oneshot;

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// A cheap, cloneable handle to the database thread.
#[derive(Clone, Debug)]
pub struct Db {
    jobs: mpsc::Sender<Job>,
}

impl Db {
    /// Opens and configures the database at `path`, applies every pending migration in one
    /// transaction (a failing migration rolls back and nothing is applied), then spawns the thread.
    pub fn open(path: &Path) -> Result<Db, StoreError> {
        let mut conn = connection::open(path)?;
        schema::migrate(&mut conn)?;
        Ok(Self::spawn(conn))
    }

    /// Hands an already configured and migrated connection to a new database thread.
    fn spawn(mut conn: Connection) -> Db {
        let (jobs, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("qm-db".into())
            .spawn(move || {
                // Ends when every sender (every Db handle) has dropped.
                for job in rx {
                    job(&mut conn);
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
        let job: Job = Box::new(move |conn| {
            let result = catch_unwind(AssertUnwindSafe(|| f(conn)))
                .unwrap_or_else(|panic| Err(StoreError::Panicked(panic_message(&*panic))));
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
