//! Mirroring Claude's transcript and restoring it.
//!
//! Claude Code resumes from its own copy, `<claude home>/projects/<project dir>/<session id>.jsonl`
//! (with a `<session id>/` folder for sub-agent transcripts), where `<claude home>` is
//! `CLAUDE_CONFIG_DIR` when the app's environment has it, else `~/.claude`, and `<project dir>` is
//! the working folder's real path with every non-alphanumeric character replaced by `-` (cut to
//! 200 characters plus a hash when longer).
//!
//! - [`mirror`] runs after every turn, before `Suspended` and before `SessionClosed`. It finds the
//!   file by session ID under `projects/*/` (never recomputing the long, hashed name), copies it
//!   and its folder into `<session_dir>/claude/` atomically, and writes `mirror.json` with the
//!   project folder it came from. A missing source is not an error.
//! - [`restore`] runs before every resume: if Claude's copy is gone and the mirror has it, it is
//!   copied back to the recorded project folder. An existing Claude copy is never overwritten.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// What `mirror.json` records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorRecord {
    pub session_id: String,
    /// The folder under `<claude home>/projects/` the transcript came from.
    pub project_dir: String,
    pub mirrored_at: Timestamp,
}

/// `<claude home>`: `CLAUDE_CONFIG_DIR` from the app's environment, else `$HOME/.claude`.
pub fn claude_home(inherited_env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(dir) = inherited_env.get(crate::env::CLAUDE_CONFIG_DIR) {
        return Some(PathBuf::from(dir));
    }
    inherited_env
        .get("HOME")
        .map(|home| Path::new(home).join(".claude"))
}

/// The mirror folder inside an agent's `session/` folder.
pub fn mirror_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("claude")
}

/// Finds `<session id>.jsonl` under `<claude home>/projects/*/`; returns the project folder name
/// and the file.
pub fn find_transcript(claude_home: &Path, session_id: &str) -> Option<(String, PathBuf)> {
    let file_name = format!("{session_id}.jsonl");
    let entries = std::fs::read_dir(claude_home.join("projects")).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(&file_name);
        if candidate.is_file() {
            return Some((entry.file_name().to_string_lossy().into_owned(), candidate));
        }
    }
    None
}

/// Copies Claude's transcript for `session_id` (and its sub-agent folder) into the mirror and
/// records where it came from. `Ok(None)` when Claude has no copy yet.
pub fn mirror(
    claude_home: &Path,
    session_id: &str,
    session_dir: &Path,
    now: Timestamp,
) -> io::Result<Option<MirrorRecord>> {
    let Some((project_dir, source)) = find_transcript(claude_home, session_id) else {
        return Ok(None);
    };
    let dest_dir = mirror_dir(session_dir);
    std::fs::create_dir_all(&dest_dir)?;
    copy_file_atomic(&source, &dest_dir.join(format!("{session_id}.jsonl")))?;

    let source_folder = source.with_file_name(session_id);
    if source_folder.is_dir() {
        let staging = dest_dir.join(format!(".{session_id}.staging-{}", std::process::id()));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        copy_dir(&source_folder, &staging)?;
        let dest_folder = dest_dir.join(session_id);
        if dest_folder.exists() {
            std::fs::remove_dir_all(&dest_folder)?;
        }
        std::fs::rename(&staging, &dest_folder)?;
    }

    let record = MirrorRecord {
        session_id: session_id.to_owned(),
        project_dir,
        mirrored_at: now,
    };
    let json = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
    write_atomic(&dest_dir.join("mirror.json"), &json)?;
    Ok(Some(record))
}

/// The mirror's record, if there is one for `session_id`.
pub fn read_record(session_dir: &Path, session_id: &str) -> Option<MirrorRecord> {
    let bytes = std::fs::read(mirror_dir(session_dir).join("mirror.json")).ok()?;
    let record: MirrorRecord = serde_json::from_slice(&bytes).ok()?;
    (record.session_id == session_id).then_some(record)
}

/// The last `cost-state` entry in Claude's transcript for `session_id`, if it has one. Claude
/// writes one as each of its processes ends and a resumed process starts its running totals
/// (`total_cost_usd`, `modelUsage`) from the last one, so a resumed process's first `result`
/// includes what the session spent before (seen live with claude 2.1.304). Read it after
/// [`restore`] and before the process starts; lines that aren't JSON are skipped.
pub fn last_cost_state(claude_home: &Path, session_id: &str) -> Option<serde_json::Value> {
    use std::io::BufRead as _;
    let (_, file) = find_transcript(claude_home, session_id)?;
    let reader = io::BufReader::new(std::fs::File::open(file).ok()?);
    reader
        .split(b'\n')
        .map_while(Result::ok)
        .filter(|line| std::str::from_utf8(line).is_ok_and(|s| s.contains("\"cost-state\"")))
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(&line).ok())
        .filter(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("cost-state"))
        .last()
}

/// Puts the mirrored transcript back if Claude's copy is gone. Returns whether anything was
/// restored. Never overwrites an existing Claude copy.
pub fn restore(claude_home: &Path, session_id: &str, session_dir: &Path) -> io::Result<bool> {
    let Some(record) = read_record(session_dir, session_id) else {
        return Ok(false);
    };
    let mirrored = mirror_dir(session_dir).join(format!("{session_id}.jsonl"));
    if !mirrored.is_file() {
        return Ok(false);
    }
    let project = claude_home.join("projects").join(&record.project_dir);
    let target = project.join(format!("{session_id}.jsonl"));
    if target.exists() || find_transcript(claude_home, session_id).is_some() {
        return Ok(false);
    }
    std::fs::create_dir_all(&project)?;
    copy_file_no_clobber(&mirrored, &target)?;

    let mirrored_folder = mirror_dir(session_dir).join(session_id);
    let target_folder = project.join(session_id);
    if mirrored_folder.is_dir() && !target_folder.exists() {
        copy_dir(&mirrored_folder, &target_folder)?;
    }
    Ok(true)
}

/// Copies through a temporary file in the destination's folder, then renames over it.
fn copy_file_atomic(from: &Path, to: &Path) -> io::Result<()> {
    let tmp = temp_beside(to);
    std::fs::copy(from, &tmp)?;
    std::fs::rename(&tmp, to).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copies through a temporary file, then hard-links it into place, which fails rather than
/// replacing a file that appeared meanwhile.
fn copy_file_no_clobber(from: &Path, to: &Path) -> io::Result<()> {
    let tmp = temp_beside(to);
    std::fs::copy(from, &tmp)?;
    let linked = std::fs::hard_link(&tmp, to);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

fn write_atomic(to: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_beside(to);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, to).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}
