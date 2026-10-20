//! The run lock: one Quartermaster per data directory.
//!
//! An exclusive advisory lock (`flock`) on `run.lock`, held for the life of the process. The
//! kernel drops it when the process exits however it exits, so a crash never leaves a stale lock.
//! The file records the holder's process ID, for people reading it; the lock itself is the truth.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Held while the app runs; dropping it releases the lock.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

/// Why the lock wasn't taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another Quartermaster is already running with this data directory")]
    AlreadyRunning,
    #[error("couldn't open {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

impl RunLock {
    /// Takes the lock without waiting.
    pub fn acquire(path: &Path) -> Result<RunLock, LockError> {
        let io_err = |source| LockError::Io {
            path: path.display().to_string(),
            source,
        };
        // Not truncated on open: a running holder's PID must survive a refused second start.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(io_err)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(LockError::AlreadyRunning),
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
        }
        file.set_len(0).map_err(io_err)?;
        writeln!(file, "{}", std::process::id()).map_err(io_err)?;
        Ok(RunLock { _file: file })
    }
}
