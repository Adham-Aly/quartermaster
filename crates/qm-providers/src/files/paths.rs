//! Path resolution and the write policy.
//!
//! A relative path resolves against the worktree. A write goes anywhere the user's account reaches,
//! except the app's own scratch folder and data folder, which only a write root inside them reopens
//! (the worktree, evidence, report, plan and scratchpad). Inside the data folder, every task's
//! worktree is open too: a crewmate works on another task's worktree when the user asks it to.
//! What stays closed is the app's own state (its database, credentials, settings, briefs and
//! sessions), never project work. Judged on real paths: the target is first
//! normalised lexically (`.` and `..` removed), then its nearest existing ancestor is
//! canonicalised (symlinks resolved) and the rest appended. The path written is that same
//! resolved path, so neither `..` nor a symlink can lead a write outside.

use std::path::{Component, Path, PathBuf};

/// The app's own scratch root (attachments, drafts, evidence). Only an explicit write root inside it
/// is writable (a crewmate's own evidence folder); no other write goes there, even though the
/// temporary folder is otherwise a write root.
const APP_SCRATCH: &str = "/tmp/quartermaster";

/// A write root inside the app's scratch root grants writes there only when it names one caller's
/// own subtree: `<uid>/<data directory>/<kind>/<name>` and deeper. A broader root (the scratch root,
/// a user's or a data directory's folder, or `/tmp` itself) grants nothing inside it.
const SCRATCH_SUBTREE_DEPTH: usize = 4;

/// The deny-only write policy: every folder is writable except the app's scratch root, which only a
/// caller's own subtree among the write roots (see [`SCRATCH_SUBTREE_DEPTH`]) can reopen.
///
/// A protected root (the app's data folder) is closed the same way: a target inside it is
/// writable only under a write root strictly inside that protected root (the worktree, the
/// evidence folder), never through the protected root itself or an ancestor of it.
#[derive(Debug, Clone)]
pub(super) struct WritePolicy {
    protected: Vec<PathBuf>,
    protected_grants: Vec<PathBuf>,
    scratch_roots: Vec<PathBuf>,
    denied: PathBuf,
}

impl WritePolicy {
    pub(super) fn new(write_roots: &[PathBuf], protected_roots: &[PathBuf]) -> Self {
        let canonical = |paths: &mut dyn Iterator<Item = PathBuf>| {
            let mut out: Vec<PathBuf> = paths.map(|root| real_path(&normalize(&root))).collect();
            out.sort();
            out.dedup();
            out
        };
        let roots = canonical(&mut write_roots.iter().cloned());
        let denied = real_path(&normalize(Path::new(APP_SCRATCH)));
        let scratch_roots = roots
            .iter()
            .filter(|root| {
                root.strip_prefix(&denied)
                    .is_ok_and(|rest| rest.components().count() >= SCRATCH_SUBTREE_DEPTH)
            })
            .cloned()
            .collect();
        let protected = canonical(&mut protected_roots.iter().cloned());
        let protected_grants = roots
            .iter()
            .filter(|root| protected.iter().any(|p| root.starts_with(p) && *root != p))
            .cloned()
            .collect();
        Self {
            protected,
            protected_grants,
            scratch_roots,
            denied,
        }
    }

    /// The real path to write for `path`, or the refusal.
    pub(super) fn check(&self, path: &Path) -> Result<PathBuf, String> {
        let target = real_path(&normalize(path));
        // The deny comes first: an ancestor root (`/tmp`, a folder above the scratch root) must not
        // reach into it.
        if target.starts_with(&self.denied) {
            if self
                .scratch_roots
                .iter()
                .any(|root| target.starts_with(root))
            {
                return Ok(target);
            }
            return Err(format!(
                "refused: {} is inside the app's own scratch folder, which no agent writes to",
                path.display()
            ));
        }
        if self.protected.iter().any(|p| target.starts_with(p)) {
            if self
                .protected_grants
                .iter()
                .any(|root| target.starts_with(root))
                || self.protected.iter().any(|p| in_a_worktree(p, &target))
            {
                return Ok(target);
            }
            return Err(format!(
                "refused: {} is the app's own state, inside its data folder; write in a \
                 worktree, your evidence, report or plan folder, or your scratchpad",
                path.display()
            ));
        }
        Ok(target)
    }
}

/// Whether `target` is inside some task's worktree under the data folder `root`:
/// `<root>/tasks/<task>/worktree/…`, strictly inside it.
fn in_a_worktree(root: &Path, target: &Path) -> bool {
    use qm_types::data_dir::{TASKS, WORKTREE};
    let Ok(rest) = target.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<&std::ffi::OsStr> = rest.components().map(Component::as_os_str).collect();
    parts.len() > 3 && parts[0] == TASKS && parts[2] == WORKTREE
}

/// `path` made absolute against `cwd`.
pub(super) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&cwd.join(p))
    }
}

/// Removes `.` and resolves `..` lexically.
pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The nearest existing ancestor canonicalised, with the rest of the path appended.
pub(super) fn real_path(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// How a path is shown in results: relative to the worktree when inside it.
pub(super) fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path)
        .display()
        .to_string()
}
