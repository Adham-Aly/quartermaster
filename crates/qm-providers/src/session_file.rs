//! JSONL session files.
//!
//! `<session_dir>/<session id>.jsonl`, the session ID a UUID v7. Append-only, one entry per line,
//! written (and so flushed: the file is unbuffered) after every entry and `fsync`ed at the end of
//! every turn:
//!
//! ```json
//! {"type":"session","v":1,"id":"…","at":"…","provider":"chatgpt","model":"gpt-…","effort":"high"}
//! {"type":"message","id":"e1","at":"…","message":{"role":"user",…}}
//! {"type":"message","id":"e2","at":"…","message":{"role":"assistant",…}}
//! {"type":"compaction","id":"e9","at":"…","summary":"…","first_kept":"e5","tokens_before":181234,"tokens_after":24310}
//! {"type":"model_change","id":"e10","at":"…","model":"gpt-…"}
//! {"type":"turn_end","id":"e11","at":"…","outcome":"completed"}
//! ```
//!
//! Assistant messages are written once complete; aborted and failed ones too, marked, and skipped
//! on replay. Resuming reads the whole file: a torn last line (a crash mid-write) is ignored with a
//! warning, unknown entry types are skipped, and a missing or unreadable file is an error (the
//! session reports `SessionClosed { ResumeFailed }`). The context is rebuilt from the latest
//! compaction ([`rebuild_context`]).

use crate::compaction::{Context, ContextItem};
use crate::transcript::Message;
use jiff::Timestamp;
use qm_types::domain::{Effort, Provider};
use qm_types::ids::ModelId;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The format version in the header.
pub const FORMAT_VERSION: u32 = 1;

/// One line of the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// The first line.
    Session {
        v: u32,
        /// The session ID.
        id: String,
        at: Timestamp,
        provider: Provider,
        model: ModelId,
        effort: Effort,
    },
    Message {
        id: String,
        at: Timestamp,
        message: Message,
    },
    Compaction {
        id: String,
        at: Timestamp,
        summary: String,
        /// The entry ID of the first message kept.
        first_kept: String,
        tokens_before: u64,
        tokens_after: u64,
    },
    ModelChange {
        id: String,
        at: Timestamp,
        model: ModelId,
    },
    TurnEnd {
        id: String,
        at: Timestamp,
        /// `completed`, `interrupted` or `failed`.
        outcome: String,
    },
}

/// What can go wrong with a session file. Messages name the file, never its contents.
#[derive(Debug, thiserror::Error)]
pub enum SessionFileError {
    #[error("the session file {path} can't be read or written: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the session file {0} doesn't exist")]
    Missing(PathBuf),
    #[error("the session file {0} has no session header")]
    NoHeader(PathBuf),
}

/// An open session file, appending.
#[derive(Debug)]
pub struct SessionFile {
    path: PathBuf,
    file: File,
    next_entry: u64,
}

/// A session file read back.
#[derive(Debug)]
pub struct LoadedSession {
    pub file: SessionFile,
    pub entries: Vec<Entry>,
}

/// The file for a session ID in a session folder.
pub fn path_for(session_dir: &Path, session_id: &str) -> PathBuf {
    session_dir.join(format!("{session_id}.jsonl"))
}

impl SessionFile {
    /// Creates a new file and writes its header.
    pub fn create(
        session_dir: &Path,
        session_id: &str,
        at: Timestamp,
        provider: Provider,
        model: ModelId,
        effort: Effort,
    ) -> Result<Self, SessionFileError> {
        let path = path_for(session_dir, session_id);
        let io = |source| SessionFileError::Io {
            path: path.clone(),
            source,
        };
        fs::create_dir_all(session_dir).map_err(io)?;
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(io)?;
        let mut this = Self {
            path: path.clone(),
            file,
            next_entry: 1,
        };
        this.write(&Entry::Session {
            v: FORMAT_VERSION,
            id: session_id.to_owned(),
            at,
            provider,
            model,
            effort,
        })?;
        Ok(this)
    }

    /// Opens an existing file for resuming: reads every entry and appends after them.
    pub fn resume(session_dir: &Path, session_id: &str) -> Result<LoadedSession, SessionFileError> {
        let path = path_for(session_dir, session_id);
        let io = |source| SessionFileError::Io {
            path: path.clone(),
            source,
        };
        let text = match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SessionFileError::Missing(path));
            }
            Err(e) => return Err(io(e)),
        };
        let entries = parse_entries(&text, &path);
        if !matches!(entries.first(), Some(Entry::Session { .. })) {
            return Err(SessionFileError::NoHeader(path));
        }
        let next_entry = entries
            .iter()
            .filter_map(entry_number)
            .max()
            .map_or(1, |n| n + 1);
        // A torn last line would glue onto the next entry: end the file with a newline first.
        let mut file = OpenOptions::new().append(true).open(&path).map_err(io)?;
        if !text.is_empty() && !text.ends_with('\n') {
            file.write_all(b"\n").map_err(io)?;
        }
        Ok(LoadedSession {
            file: Self {
                path,
                file,
                next_entry,
            },
            entries,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The next entry's ID (`e<n>`), reserved.
    pub fn next_id(&mut self) -> String {
        let id = format!("e{}", self.next_entry);
        self.next_entry += 1;
        id
    }

    /// Appends one entry as one line.
    pub fn write(&mut self, entry: &Entry) -> Result<(), SessionFileError> {
        let mut line = serde_json::to_vec(entry).map_err(|e| SessionFileError::Io {
            path: self.path.clone(),
            source: std::io::Error::other(e),
        })?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .map_err(|source| SessionFileError::Io {
                path: self.path.clone(),
                source,
            })
    }

    /// Appends a message and returns its entry ID.
    pub fn append_message(
        &mut self,
        at: Timestamp,
        message: &Message,
    ) -> Result<String, SessionFileError> {
        let id = self.next_id();
        self.write(&Entry::Message {
            id: id.clone(),
            at,
            message: message.clone(),
        })?;
        Ok(id)
    }

    /// `fsync`s the file (end of every turn, and on close).
    pub fn sync(&self) -> Result<(), SessionFileError> {
        self.file
            .sync_data()
            .map_err(|source| SessionFileError::Io {
                path: self.path.clone(),
                source,
            })
    }
}

fn entry_number(entry: &Entry) -> Option<u64> {
    let id = match entry {
        Entry::Session { .. } => return None,
        Entry::Message { id, .. }
        | Entry::Compaction { id, .. }
        | Entry::ModelChange { id, .. }
        | Entry::TurnEnd { id, .. } => id,
    };
    id.strip_prefix('e')?.parse().ok()
}

/// Every entry of a file's text. A last line that doesn't parse is a torn write and is ignored
/// with a warning; an unknown entry type, or a damaged line in the middle, is skipped with one.
pub fn parse_entries(text: &str, path: &Path) -> Vec<Entry> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entries = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => entries.push(entry),
            Err(_) if n + 1 == lines.len() => {
                tracing::warn!(
                    path = %path.display(),
                    "the session file's last line is incomplete (a crash mid-write); ignoring it"
                );
            }
            Err(_) => {
                let known = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned));
                tracing::warn!(
                    path = %path.display(),
                    line = n + 1,
                    entry_type = known.as_deref().unwrap_or("unreadable"),
                    "skipping a session file line this build doesn't read"
                );
            }
        }
    }
    entries
}

/// The context a file's entries describe: the latest compaction's summary, then every message
/// from its first kept entry on (or every message, with no compaction). Also the latest model
/// change, if any.
pub fn rebuild_context(entries: &[Entry]) -> (Context, Option<ModelId>) {
    let latest_compaction = entries.iter().enumerate().rev().find_map(|(i, e)| match e {
        Entry::Compaction {
            summary,
            first_kept,
            ..
        } => Some((i, summary.clone(), first_kept.clone())),
        _ => None,
    });
    let messages: Vec<(usize, &String, &Message)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Entry::Message { id, message, .. } => Some((i, id, message)),
            _ => None,
        })
        .collect();
    let mut context = Context::default();
    let kept: Vec<&(usize, &String, &Message)> = match &latest_compaction {
        Some((at, summary, first_kept)) => {
            context.summary = Some(summary.clone());
            match messages.iter().position(|(_, id, _)| *id == first_kept) {
                Some(start) => messages[start..].iter().collect(),
                None => messages.iter().filter(|(i, _, _)| i > at).collect(),
            }
        }
        None => messages.iter().collect(),
    };
    context.items = kept
        .into_iter()
        .map(|(_, id, message)| ContextItem {
            entry_id: (*id).clone(),
            message: (*message).clone(),
        })
        .collect();
    let model = entries.iter().rev().find_map(|e| match e {
        Entry::ModelChange { model, .. } => Some(model.clone()),
        _ => None,
    });
    (context, model)
}
