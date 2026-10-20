//! One writer per repository inside the app.
//!
//! Git takes its own lock files for each ref and the worktree list, and fails (rather than
//! waits) when two commands collide: two tasks of one project fetching or retiring at the same
//! moment would see "cannot lock ref". Every operation here that changes a repository holds that
//! repository's lock, keyed by its common git directory, so the app's own commands queue up.
//! Crewmates' git commands are separate processes and are not affected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

type Registry = Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// The in-process lock of the repository whose common git directory is `common_dir`.
pub(crate) fn repo_lock(common_dir: &Path) -> Arc<Mutex<()>> {
    let mut map = registry().lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(map.entry(common_dir.to_owned()).or_default())
}

/// Holds `lock` for the guard's lifetime. A panic in an earlier holder doesn't poison it: every
/// step here re-reads git's state rather than trusting memory.
pub(crate) fn hold(lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}
