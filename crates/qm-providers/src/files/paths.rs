//! Path resolution and the write policy.
//!
//! A relative path resolves against the worktree. A write is allowed only when its target lies
//! under a write root or the system temporary folder, judged on real paths: the target is first
//! normalised lexically (`.` and `..` removed), then its nearest existing ancestor is
//! canonicalised (symlinks resolved) and the rest appended. The path written is that same
//! resolved path, so neither `..` nor a symlink can lead a write outside.

use std::path::{Component, Path, PathBuf};

/// The app's own scratch root (attachments, drafts, evidence). Only an explicit write root inside it
/// is writable (a crewmate's own evidence folder); no other write goes there, even though the
/// temporary folder is otherwise a write root.
const APP_SCRATCH: &str = "/tmp/quartermaster";

/// The folders writes may go to, canonicalised: the explicit write roots, then the temporary
/// folders, minus the app's scratch root.
#[derive(Debug, Clone)]
pub(super) struct WritePolicy {
    roots: Vec<PathBuf>,
    temp: Vec<PathBuf>,
    denied: PathBuf,
}

impl WritePolicy {
    pub(super) fn new(write_roots: &[PathBuf]) -> Self {
        let canonical = |paths: &mut dyn Iterator<Item = PathBuf>| {
            let mut out: Vec<PathBuf> = paths.map(|root| real_path(&normalize(&root))).collect();
            out.sort();
            out.dedup();
            out
        };
        let roots = canonical(&mut write_roots.iter().cloned());
        let temp = canonical(&mut [std::env::temp_dir(), PathBuf::from("/tmp")].into_iter());
        let denied = real_path(&normalize(Path::new(APP_SCRATCH)));
        Self {
            roots,
            temp,
            denied,
        }
    }

    /// The real path to write for `path`, or the refusal.
    pub(super) fn check(&self, path: &Path) -> Result<PathBuf, String> {
        let target = real_path(&normalize(path));
        if self.roots.iter().any(|root| target.starts_with(root)) {
            Ok(target)
        } else if target.starts_with(&self.denied) {
            Err(format!(
                "refused: {} is inside the app's own scratch folder, which no agent writes to",
                path.display()
            ))
        } else if self.temp.iter().any(|root| target.starts_with(root)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apps_scratch_folder_refuses_writes_even_inside_the_temporary_folder() {
        let policy = WritePolicy::new(&[]);
        let scratch = Path::new(APP_SCRATCH).join("1000/abc/evidence/run.log");
        assert!(policy.check(&scratch).is_err());
        assert!(policy.check(Path::new("/tmp/elsewhere/run.log")).is_ok());
    }

    #[test]
    fn a_crewmates_own_evidence_folder_inside_the_scratch_root_is_writable() {
        let evidence = Path::new(APP_SCRATCH).join("1000/abc/evidence/T-0042");
        let policy = WritePolicy::new(std::slice::from_ref(&evidence));
        assert!(policy.check(&evidence.join("run.log")).is_ok());
        assert!(
            policy
                .check(&Path::new(APP_SCRATCH).join("1000/abc/attachments/x.png"))
                .is_err()
        );
    }

    #[test]
    fn a_dot_dot_into_the_scratch_folder_is_still_refused() {
        let policy = WritePolicy::new(&[]);
        let sneaky = Path::new("/tmp/notes/../quartermaster/1000/run.log");
        assert!(policy.check(sneaky).is_err());
        let out = Path::new("/tmp/quartermaster/1000/../../notes/run.log");
        assert!(
            policy.check(out).is_ok(),
            "a path that leaves the scratch folder is an ordinary /tmp write"
        );
    }
}
