//! Path resolution and the write policy.
//!
//! A relative path resolves against the worktree. A write is allowed only when its target lies
//! under a write root or the system temporary folder, judged on real paths: the target is first
//! normalised lexically (`.` and `..` removed), then its nearest existing ancestor is
//! canonicalised (symlinks resolved) and the rest appended. The path written is that same
//! resolved path, so neither `..` nor a symlink can lead a write outside.

use std::path::{Component, Path, PathBuf};

/// The folders writes may go to, canonicalised.
#[derive(Debug, Clone)]
pub(super) struct WritePolicy {
    roots: Vec<PathBuf>,
}

impl WritePolicy {
    pub(super) fn new(write_roots: &[PathBuf]) -> Self {
        let mut roots: Vec<PathBuf> = write_roots
            .iter()
            .cloned()
            .chain([std::env::temp_dir(), PathBuf::from("/tmp")])
            .map(|root| real_path(&normalize(&root)))
            .collect();
        roots.sort();
        roots.dedup();
        Self { roots }
    }

    /// The real path to write for `path`, or the refusal.
    pub(super) fn check(&self, path: &Path) -> Result<PathBuf, String> {
        let target = real_path(&normalize(path));
        if self.roots.iter().any(|root| target.starts_with(root)) {
            Ok(target)
        } else {
            Err(format!(
                "refused: {} is outside the worktree, the evidence folder and the temporary folder",
                path.display()
            ))
        }
    }
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
