//! Reading Claude's own transcript.
//!
//! Claude Code resumes from its own copy, `<claude home>/projects/<project dir>/<session id>.jsonl`
//! (with a `<session id>/` folder for sub-agent transcripts), where `<claude home>` is
//! `CLAUDE_CONFIG_DIR` when the app's environment has it, else `~/.claude`, and `<project dir>` is
//! the working folder's real path with every non-alphanumeric character replaced by `-` (cut to
//! 200 characters plus a hash when longer).
//!
//! The app keeps no copy of it: the transcript is Claude's to keep. The one thing read from it is
//! the running cost totals a resumed process starts from ([`last_cost_state`]).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// `<claude home>`: `CLAUDE_CONFIG_DIR` from the app's environment, else `$HOME/.claude`.
pub fn claude_home(inherited_env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(dir) = inherited_env.get(crate::env::CLAUDE_CONFIG_DIR) {
        return Some(PathBuf::from(dir));
    }
    inherited_env
        .get("HOME")
        .map(|home| Path::new(home).join(".claude"))
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

/// The last `cost-state` entry in Claude's transcript for `session_id`, if it has one. Claude
/// writes one as each of its processes ends and a resumed process starts its running totals
/// (`total_cost_usd`, `modelUsage`) from the last one, so a resumed process's first `result`
/// includes what the session spent before (seen live with claude 2.1.304). Read it before the
/// process starts; lines that aren't JSON are skipped.
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
