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
