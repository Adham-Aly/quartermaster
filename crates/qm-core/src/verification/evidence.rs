//! The evidence folder.
//!
//! `tasks/T-0042/evidence/` is the crewmate's `QM_EVIDENCE_DIR`. [`manifest`] lists what is in it
//! (relative paths, sizes and times, at most [`MAX_ENTRIES`], symlinks not followed); every clear
//! done report records it as a `task.evidence` event, and the review brief lists it so the
//! reviewer can scrutinise it.

use jiff::Timestamp;
use qm_types::data_dir::TaskPaths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// The most files a manifest lists.
pub const MAX_ENTRIES: usize = 200;
/// `events.kind` of a done report's evidence manifest.
pub const EVIDENCE_EVENT: &str = "task.evidence";

/// One file in the evidence folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceFile {
    /// Relative to the evidence folder.
    pub path: String,
    pub bytes: u64,
    pub modified: Option<Timestamp>,
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<EvidenceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        // `symlink_metadata`: a link is listed as itself, never followed.
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            walk(root, &entry.path(), out);
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)
            .map_or_else(|_| entry.path(), Path::to_path_buf)
            .to_string_lossy()
            .into_owned();
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| Timestamp::try_from(t).ok());
        out.push(EvidenceFile {
            path,
            bytes: meta.len(),
            modified,
        });
    }
}

/// What is in the task's evidence folder, sorted by path, at most [`MAX_ENTRIES`] files
/// (blocking: reads the file system).
pub fn manifest(paths: &TaskPaths) -> Vec<EvidenceFile> {
    let mut out = Vec::new();
    walk(&paths.evidence, &paths.evidence, &mut out);
    out
}

/// The manifest as the review brief lists it: the folder's absolute path, then one line per file.
pub fn brief_lines(paths: &TaskPaths, files: &[EvidenceFile]) -> Vec<String> {
    let folder = paths.evidence.display();
    if files.is_empty() {
        return vec![format!("Evidence: none in {folder}.")];
    }
    let mut out = vec![format!(
        "Evidence in {folder} ({} files; scrutinise it against Verified):",
        files.len()
    )];
    out.extend(
        files
            .iter()
            .map(|f| format!("- {} ({} bytes)", f.path, f.bytes)),
    );
    out
}
