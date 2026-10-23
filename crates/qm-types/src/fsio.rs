//! Durable file writes.
//!
//! The rule every caller relies on: a file is complete on disk before any row names it, and a
//! crash at any instant leaves either the old file or the new one, never a torn mix.
//!
//! - [`write_atomic`] replaces a whole file: a private temporary file beside it (created with
//!   `create_new`, mode 0600) is written and synced, renamed over the target, and the folder is
//!   synced so the rename itself survives a power cut.
//! - [`AppendLog`] adds whole lines to a file opened with `O_APPEND`; [`AppendLog::sync`] makes
//!   them durable.
//!
//! Leftover temporary files are named `.<name>.tmp-<pid>-<n>`; the retention sweep removes old
//! ones ([`is_temporary`]).

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The marker in a temporary file's name.
const TMP_MARK: &str = ".tmp-";

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `bytes`, atomically and durably, mode 0600.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, bytes, |from, to| fs::rename(from, to))
}

fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = parent_of(path)?;
    let tmp = temporary_beside(path)?;
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    fsync_dir(parent)
}

/// Syncs a folder, so the names added, renamed or removed in it survive a power cut.
pub fn fsync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Whether `name` is one of [`write_atomic`]'s temporary files.
pub fn is_temporary(name: &str) -> bool {
    name.starts_with('.') && name.contains(TMP_MARK)
}

fn parent_of(path: &Path) -> io::Result<&Path> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => Ok(p),
        _ => Err(io::Error::other("a file to replace needs a parent folder")),
    }
}

fn temporary_beside(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("a file to replace needs a plain name"))?;
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    Ok(parent_of(path)?.join(format!(".{name}{TMP_MARK}{}-{n}", std::process::id())))
}

/// A file that grows by whole lines.
#[derive(Debug)]
pub struct AppendLog {
    file: fs::File,
}

impl AppendLog {
    /// Opens `path` for appending, creating it with mode 0600 if absent. A file this creates is
    /// made durable (its folder synced) before this returns.
    pub fn open(path: &Path) -> io::Result<Self> {
        let existed = match fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_file() => {
                return Err(io::Error::other("a log must be a plain file, not a link"));
            }
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        let file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        if !existed {
            file.sync_all()?;
            fsync_dir(parent_of(path)?)?;
        }
        Ok(Self { file })
    }

    /// Appends `line` and a newline in one write. `line` must not contain a newline.
    pub fn append_line(&mut self, line: &[u8]) -> io::Result<()> {
        if line.contains(&b'\n') {
            return Err(io::Error::other("a log line can't contain a newline"));
        }
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line);
        buf.push(b'\n');
        self.file.write_all(&buf)
    }

    /// Makes every appended line durable.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("qm-fsio-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("a folder");
            Self(path)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn write_atomic_replaces_whole_file_privately() {
        let folder = Folder::new("replace");
        let path = folder.0.join("plan.md");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(names(&folder.0), ["plan.md"]);
    }

    #[test]
    fn interrupted_replace_keeps_the_old_file_and_no_temporary() {
        let folder = Folder::new("interrupted");
        let path = folder.0.join("t.jsonl");
        write_atomic(&path, b"old").unwrap();
        let err = write_atomic_with(&path, b"new", |_, _| Err(io::Error::other("power cut")));
        assert!(err.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(names(&folder.0), ["t.jsonl"]);
    }

    #[test]
    fn temporary_names_are_recognised() {
        let folder = Folder::new("names");
        let tmp = temporary_beside(&folder.0.join("a.jsonl")).unwrap();
        let name = tmp.file_name().unwrap().to_str().unwrap();
        assert!(is_temporary(name));
        assert!(!is_temporary("a.jsonl"));
        assert!(!is_temporary(".hidden"));
    }

    #[test]
    fn append_log_adds_whole_lines_and_refuses_links() {
        let folder = Folder::new("append");
        let path = folder.0.join("log");
        let mut log = AppendLog::open(&path).unwrap();
        log.append_line(b"one").unwrap();
        log.append_line(b"two").unwrap();
        log.sync().unwrap();
        assert!(log.append_line(b"a\nb").is_err());
        drop(log);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let link = folder.0.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(AppendLog::open(&link).is_err());
    }
}
