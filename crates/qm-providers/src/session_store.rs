//! Provider session history: in memory, and on disk for the sessions that have a transcript
//! folder.
//!
//! Each session's entries live in one process-wide map, keyed by its session ID (a UUID v7); the
//! map is the working copy. A session opened with a transcript folder (`SessionSpec::transcript_dir`:
//! the Quartermaster's, a secondmate's and a crewmate's) also has `<folder>/<id>.jsonl`, one entry
//! per line, so it can be resumed after the app quits, crashes or the Mac restarts. Helpers have no
//! folder; their history is in memory only, and is lost with the app by design.
//!
//! The first entry is the header; every entry after it is a message, a compaction, a model change
//! or a turn end:
//!
//! ```text
//! {"type":"session","v":1,"id":"…","at":…,"provider":…,"model":…,"effort":…}
//! {"type":"message","id":"e1","at":…,"message":{"role":"user",…}}
//! {"type":"message","id":"e2","at":…,"message":{"role":"assistant",…}}
//! {"type":"compaction","id":"e9","at":…,"summary":…,"first_kept":"e5","tokens_before":…,"tokens_after":…}
//! {"type":"model_change","id":"e10","at":…,"model":…}
//! {"type":"turn_end","id":"e11","at":…,"outcome":"completed"}
//! ```
//!
//! Assistant messages are recorded once complete; aborted and failed ones too, marked, and skipped
//! on replay. A compaction drops the entries before its first kept message, so the history holds
//! the header and what the next turn can see. The context is rebuilt from the latest compaction
//! ([`rebuild_context`]).
//!
//! **On disk.** Creating a session writes the header atomically (`qm_types::fsio::write_atomic`).
//! Each later entry is appended as one whole line at once, so a killed app loses nothing written;
//! a turn end and a model change also sync the file, so a power cut loses at most the turn in
//! flight. A compaction replaces the whole file atomically with the compacted history before the
//! engine reports it. [`SessionStore::forget`] (a cleared, retired or closed conversation) deletes
//! the file; a session that merely stops (the app quits, a task panics) leaves it to resume. A
//! file that can't be written is logged and rewritten whole at the next entry; memory stays right.
//!
//! **Damage.** A torn last line (no newline, or not parseable) is a write the app didn't finish:
//! [`SessionStore::resume`] drops it and repairs the file. A wrong header or a bad line before the
//! last is damage it can't repair: resume fails, the core opens a fresh session, and the app's
//! retention sweep removes the old file once nothing names it.

use crate::compaction::{Context, ContextItem};
use crate::transcript::Message;
use jiff::Timestamp;
use qm_types::domain::{Effort, Provider};
use qm_types::fsio::{self, AppendLog};
use qm_types::ids::ModelId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

/// The format version in the header.
pub const FORMAT_VERSION: u32 = 1;

/// A transcript file's extension.
pub const TRANSCRIPT_EXTENSION: &str = "jsonl";

/// One entry of a session's history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// The first entry.
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

/// What can go wrong with a session's history.
#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("the session {0} has no history (it was forgotten or never started)")]
    Missing(String),
    #[error("the session {id}'s transcript is damaged ({problem}), so it can't be resumed")]
    Damaged { id: String, problem: String },
    #[error("the session {id}'s transcript couldn't be read: {source}")]
    Unreadable {
        id: String,
        #[source]
        source: io::Error,
    },
}

/// One session's history and, when it has one, its file.
#[derive(Debug)]
struct Held {
    entries: Vec<Entry>,
    disk: Option<Disk>,
    /// Set by [`SessionStore::forget`]: later writes fail, as for an unknown session.
    forgotten: bool,
}

/// A session's transcript file.
#[derive(Debug)]
struct Disk {
    path: PathBuf,
    /// Open for appending; `None` until the file is known whole on disk.
    log: Option<AppendLog>,
}

impl Disk {
    /// Replaces the file with every entry, then reopens it for appending.
    fn rewrite(&mut self, entries: &[Entry]) -> io::Result<()> {
        self.log = None;
        fsio::write_atomic(&self.path, &encode(entries)?)?;
        self.log = Some(AppendLog::open(&self.path)?);
        Ok(())
    }

    /// Adds `entry` (already last in `entries`), syncing when `sync`. A file that missed an
    /// earlier write is rewritten whole instead.
    fn append(&mut self, entries: &[Entry], entry: &Entry, sync: bool) -> io::Result<()> {
        let Some(log) = self.log.as_mut() else {
            return self.rewrite(entries);
        };
        let line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        let result = log
            .append_line(&line)
            .and_then(|()| if sync { log.sync() } else { Ok(()) });
        if result.is_err() {
            // The line may be half written: the next write replaces the whole file.
            self.log = None;
        }
        result
    }

    fn report(&self, what: &str, result: io::Result<()>) {
        if let Err(e) = result {
            tracing::error!(
                path = %self.path.display(),
                error = %e,
                "couldn't {what} a transcript; it is rewritten at the next entry"
            );
        }
    }
}

fn encode(entries: &[Entry]) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

type Shared = Arc<Mutex<Held>>;

/// Every session's history in this process, by session ID.
static SESSIONS: LazyLock<Mutex<HashMap<String, Shared>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sessions() -> MutexGuard<'static, HashMap<String, Shared>> {
    // A panic elsewhere can't corrupt a map insert, so a poisoned lock still holds good history.
    SESSIONS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock(held: &Shared) -> MutexGuard<'_, Held> {
    held.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The transcript file of session `id` in `dir`, if `id` is a plain session ID.
pub fn transcript_path(dir: &Path, id: &str) -> Option<PathBuf> {
    let plain = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    plain.then(|| dir.join(format!("{id}.{TRANSCRIPT_EXTENSION}")))
}

/// A handle on one session's history, appending.
#[derive(Debug)]
pub struct SessionStore {
    session_id: String,
    next_entry: u64,
    held: Shared,
}

/// A session's history read back.
#[derive(Debug)]
pub struct LoadedSession {
    pub store: SessionStore,
    pub entries: Vec<Entry>,
}

impl SessionStore {
    /// Starts a session's history with its header, in `dir` too when given.
    pub fn create(
        session_id: &str,
        at: Timestamp,
        provider: Provider,
        model: ModelId,
        effort: Effort,
        dir: Option<&Path>,
    ) -> Self {
        let header = Entry::Session {
            v: FORMAT_VERSION,
            id: session_id.to_owned(),
            at,
            provider,
            model,
            effort,
        };
        let entries = vec![header];
        let disk = dir
            .and_then(|dir| transcript_path(dir, session_id))
            .map(|path| {
                let mut disk = Disk { path, log: None };
                let result = disk.rewrite(&entries);
                disk.report("create", result);
                disk
            });
        let held = Arc::new(Mutex::new(Held {
            entries,
            disk,
            forgotten: false,
        }));
        sessions().insert(session_id.to_owned(), Arc::clone(&held));
        Self {
            session_id: session_id.to_owned(),
            next_entry: 1,
            held,
        }
    }

    /// Reads a session's history back, to continue it: from memory if this run still holds it,
    /// otherwise from its file in `dir`.
    pub fn resume(
        session_id: &str,
        dir: Option<&Path>,
    ) -> Result<LoadedSession, SessionStoreError> {
        let held = sessions().get(session_id).cloned();
        let held = match held {
            Some(held) => held,
            None => {
                let path = dir
                    .and_then(|dir| transcript_path(dir, session_id))
                    .ok_or_else(|| SessionStoreError::Missing(session_id.to_owned()))?;
                let entries = load(&path, session_id)?;
                let held = Arc::new(Mutex::new(Held {
                    entries,
                    disk: Some(Disk { path, log: None }),
                    forgotten: false,
                }));
                {
                    let mut guard = lock(&held);
                    let Held { entries, disk, .. } = &mut *guard;
                    if let Some(disk) = disk {
                        // Reopen for appending, or repair a torn tail by rewriting.
                        let result = disk.rewrite(entries);
                        disk.report("reopen", result);
                    }
                }
                sessions()
                    .entry(session_id.to_owned())
                    .or_insert(held)
                    .clone()
            }
        };
        let entries = lock(&held).entries.clone();
        let next_entry = entries
            .iter()
            .filter_map(entry_number)
            .max()
            .map_or(1, |n| n + 1);
        Ok(LoadedSession {
            store: Self {
                session_id: session_id.to_owned(),
                next_entry,
                held,
            },
            entries,
        })
    }

    /// Forgets a session's history: memory and file. A later resume of it fails with
    /// [`SessionStoreError::Missing`].
    pub fn forget(session_id: &str) {
        let Some(held) = sessions().remove(session_id) else {
            return;
        };
        let mut held = lock(&held);
        held.forgotten = true;
        held.entries.clear();
        if let Some(disk) = held.disk.take() {
            drop(disk.log);
            let result = qm_types::data_dir::remove_path(&disk.path)
                .and_then(|()| disk.path.parent().map_or(Ok(()), fsio::fsync_dir));
            if let Err(e) = result {
                tracing::error!(path = %disk.path.display(), error = %e, "couldn't delete a forgotten transcript");
            }
        }
    }

    /// Lets go of a session's history in memory, keeping its file to resume from later.
    pub fn release(session_id: &str) {
        sessions().remove(session_id);
    }

    /// The next entry's ID (`e<n>`), reserved.
    pub fn next_id(&mut self) -> String {
        let id = format!("e{}", self.next_entry);
        self.next_entry += 1;
        id
    }

    /// Appends one entry. A compaction then drops the entries before its first kept message and
    /// replaces the file with what is left.
    pub fn write(&mut self, entry: &Entry) -> Result<(), SessionStoreError> {
        let mut guard = lock(&self.held);
        if guard.forgotten {
            return Err(SessionStoreError::Missing(self.session_id.clone()));
        }
        let Held { entries, disk, .. } = &mut *guard;
        entries.push(entry.clone());
        match entry {
            Entry::Compaction { first_kept, .. } => {
                keep_from(entries, first_kept);
                if let Some(disk) = disk {
                    let result = disk.rewrite(entries);
                    disk.report("compact", result);
                }
            }
            Entry::TurnEnd { .. } | Entry::ModelChange { .. } => {
                if let Some(disk) = disk {
                    let result = disk.append(entries, entry, true);
                    disk.report("write", result);
                }
            }
            Entry::Session { .. } | Entry::Message { .. } => {
                if let Some(disk) = disk {
                    let result = disk.append(entries, entry, false);
                    disk.report("write", result);
                }
            }
        }
        Ok(())
    }

    /// Appends a message and returns its entry ID.
    pub fn append_message(
        &mut self,
        at: Timestamp,
        message: &Message,
    ) -> Result<String, SessionStoreError> {
        let id = self.next_id();
        self.write(&Entry::Message {
            id: id.clone(),
            at,
            message: message.clone(),
        })?;
        Ok(id)
    }
}

/// Reads a transcript file: its entries, without a torn last line.
fn load(path: &Path, id: &str) -> Result<Vec<Entry>, SessionStoreError> {
    let damaged = |problem: &str| SessionStoreError::Damaged {
        id: id.to_owned(),
        problem: problem.to_owned(),
    };
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SessionStoreError::Missing(id.to_owned()));
        }
        Err(source) => {
            return Err(SessionStoreError::Unreadable {
                id: id.to_owned(),
                source,
            });
        }
    };
    if !meta.is_file() {
        return Err(damaged("not a plain file"));
    }
    let bytes = std::fs::read(path).map_err(|source| SessionStoreError::Unreadable {
        id: id.to_owned(),
        source,
    })?;
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    // After the last newline: empty when the file ends whole, a torn write otherwise.
    lines.pop();
    let mut entries = Vec::with_capacity(lines.len());
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_slice::<Entry>(line) {
            Ok(entry) => entries.push(entry),
            // A line the app didn't finish writing before it stopped.
            Err(_) if i == last && i > 0 => break,
            Err(_) => return Err(damaged(&format!("line {} can't be read", i + 1))),
        }
    }
    match entries.first() {
        Some(Entry::Session { v, id: header, .. }) if *v == FORMAT_VERSION && header == id => {
            Ok(entries)
        }
        Some(Entry::Session { .. }) => Err(damaged("its header names another session or format")),
        _ => Err(damaged("it has no header")),
    }
}

/// Keeps the header and every entry from the message `first_kept` on. A `first_kept` that isn't
/// in the history keeps everything, rather than losing the context.
fn keep_from(entries: &mut Vec<Entry>, first_kept: &str) {
    let Some(start) = entries
        .iter()
        .position(|e| matches!(e, Entry::Message { id, .. } if id == first_kept))
    else {
        return;
    };
    let tail = entries.split_off(start);
    entries.truncate(1);
    entries.extend(tail);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::ContextItem;

    fn start(id: &str) -> SessionStore {
        let model = ModelId::new("gpt-test").expect("a model id");
        let at = Timestamp::from_second(1).expect("a time");
        SessionStore::create(id, at, Provider::ChatGpt, model, Effort::Medium, None)
    }

    /// A throwaway transcript folder, removed when dropped.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("qm-transcript-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a folder");
            Self(path)
        }

        fn file(&self, id: &str) -> PathBuf {
            transcript_path(&self.0, id).expect("a plain id")
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn start_on_disk(id: &str, folder: &Folder) -> SessionStore {
        let model = ModelId::new("gpt-test").expect("a model id");
        let at = Timestamp::from_second(1).expect("a time");
        SessionStore::create(
            id,
            at,
            Provider::ChatGpt,
            model,
            Effort::Medium,
            Some(&folder.0),
        )
    }

    fn texts(entries: &[Entry]) -> Vec<String> {
        let (context, _) = rebuild_context(entries);
        context
            .items
            .iter()
            .map(|i| serde_json::to_string(&i.message).expect("json"))
            .collect()
    }

    fn turn_end(store: &mut SessionStore) {
        let id = store.next_id();
        let at = Timestamp::from_second(3).expect("a time");
        store
            .write(&Entry::TurnEnd {
                id,
                at,
                outcome: "completed".into(),
            })
            .unwrap();
    }

    #[test]
    fn a_transcript_on_disk_resumes_after_the_app_lets_go_of_it() {
        let folder = Folder::new("restart");
        let id = "0190-restart";
        let mut store = start_on_disk(id, &folder);
        let at = Timestamp::from_second(2).expect("a time");
        store
            .append_message(at, &Message::user_text("remember PELICAN"))
            .unwrap();
        turn_end(&mut store);
        let before = SessionStore::resume(id, None).unwrap().entries;
        // The app stops: memory goes, the file stays.
        drop(store);
        SessionStore::release(id);
        assert!(SessionStore::resume(id, None).is_err(), "nothing in memory");
        let loaded = SessionStore::resume(id, Some(&folder.0)).expect("resumes from disk");
        assert_eq!(loaded.entries, before);
        assert!(texts(&loaded.entries)[0].contains("PELICAN"));
        let mut resumed = loaded.store;
        assert_eq!(resumed.next_id(), "e3");
        SessionStore::forget(id);
        assert!(!folder.file(id).exists(), "forget deletes the file");
        assert!(matches!(
            SessionStore::resume(id, Some(&folder.0)),
            Err(SessionStoreError::Missing(_))
        ));
    }

    #[test]
    fn a_torn_last_line_is_dropped_and_the_file_repaired() {
        let folder = Folder::new("torn");
        let id = "0190-torn";
        let mut store = start_on_disk(id, &folder);
        let at = Timestamp::from_second(2).expect("a time");
        store
            .append_message(at, &Message::user_text("whole"))
            .unwrap();
        drop(store);
        SessionStore::release(id);
        let path = folder.file(id);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(br#"{"type":"message","id":"e2","at":"#);
        std::fs::write(&path, &bytes).unwrap();

        let loaded = SessionStore::resume(id, Some(&folder.0)).expect("a torn tail is repaired");
        assert_eq!(loaded.entries.len(), 2, "the header and the whole message");
        let repaired = std::fs::read_to_string(&path).unwrap();
        assert!(repaired.ends_with('\n'));
        assert_eq!(repaired.lines().count(), 2);
        let mut store = loaded.store;
        store
            .append_message(at, &Message::user_text("after"))
            .unwrap();
        drop(store);
        SessionStore::release(id);
        let again = SessionStore::resume(id, Some(&folder.0)).unwrap();
        assert_eq!(texts(&again.entries).len(), 2);
        SessionStore::forget(id);
    }

    #[test]
    fn damage_before_the_last_line_or_a_wrong_header_refuses_to_resume_and_keeps_the_file() {
        let folder = Folder::new("damaged");
        let id = "0190-damaged";
        let mut store = start_on_disk(id, &folder);
        let at = Timestamp::from_second(2).expect("a time");
        store
            .append_message(at, &Message::user_text("one"))
            .unwrap();
        store
            .append_message(at, &Message::user_text("two"))
            .unwrap();
        drop(store);
        SessionStore::release(id);
        let path = folder.file(id);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "not json";
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert!(matches!(
            SessionStore::resume(id, Some(&folder.0)),
            Err(SessionStoreError::Damaged { .. })
        ));
        assert!(path.exists(), "the engine never deletes a damaged file");

        let other = "0190-other";
        std::fs::write(
            folder.file(other),
            text.lines().next().unwrap().to_owned() + "\n",
        )
        .unwrap();
        assert!(matches!(
            SessionStore::resume(other, Some(&folder.0)),
            Err(SessionStoreError::Damaged { .. })
        ));
        assert!(transcript_path(&folder.0, "../escape").is_none());
    }

    #[test]
    fn a_compaction_replaces_the_file_with_the_compacted_history() {
        let folder = Folder::new("compact");
        let id = "0190-compact";
        let mut store = start_on_disk(id, &folder);
        let at = Timestamp::from_second(2).expect("a time");
        for text in ["old", "kept", "newest"] {
            store.append_message(at, &Message::user_text(text)).unwrap();
        }
        let path = folder.file(id);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 4);
        let cid = store.next_id();
        store
            .write(&Entry::Compaction {
                id: cid,
                at,
                summary: "the summary".to_owned(),
                first_kept: "e2".to_owned(),
                tokens_before: 100,
                tokens_after: 10,
            })
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().count(),
            4,
            "header, two kept messages, the compaction"
        );
        assert!(
            !text.contains("\"old\""),
            "the compacted-away message is gone from disk"
        );
        store
            .append_message(at, &Message::user_text("later"))
            .unwrap();
        drop(store);
        SessionStore::release(id);
        let loaded = SessionStore::resume(id, Some(&folder.0)).unwrap();
        let (context, _) = rebuild_context(&loaded.entries);
        assert_eq!(context.summary.as_deref(), Some("the summary"));
        assert_eq!(context.items.len(), 3);
        SessionStore::forget(id);
    }

    #[test]
    fn a_history_resumes_in_this_run_and_continues_its_numbers() {
        let mut store = start("history-resumes");
        let at = Timestamp::from_second(2).expect("a time");
        assert_eq!(
            store
                .append_message(at, &Message::user_text("one"))
                .unwrap(),
            "e1"
        );
        assert_eq!(
            store
                .append_message(at, &Message::user_text("two"))
                .unwrap(),
            "e2"
        );

        let loaded = SessionStore::resume("history-resumes", None).expect("the history is held");
        assert_eq!(loaded.entries.len(), 3, "the header and two messages");
        let mut resumed = loaded.store;
        assert_eq!(resumed.next_id(), "e3");
        SessionStore::forget("history-resumes");
    }

    #[test]
    fn a_compaction_drops_the_entries_before_its_first_kept_message() {
        let mut store = start("compaction-drops");
        let at = Timestamp::from_second(2).expect("a time");
        for text in ["old", "kept", "newest"] {
            store.append_message(at, &Message::user_text(text)).unwrap();
        }
        let id = store.next_id();
        store
            .write(&Entry::Compaction {
                id,
                at,
                summary: "the summary".to_owned(),
                first_kept: "e2".to_owned(),
                tokens_before: 100,
                tokens_after: 10,
            })
            .unwrap();

        let loaded = SessionStore::resume("compaction-drops", None).expect("the history is held");
        let ids: Vec<&str> = loaded
            .entries
            .iter()
            .skip(1)
            .map(|e| match e {
                Entry::Message { id, .. } | Entry::Compaction { id, .. } => id.as_str(),
                _ => "other",
            })
            .collect();
        assert_eq!(
            ids,
            ["e2", "e3", "e4"],
            "the header, then from the kept message on"
        );
        let (context, _model) = rebuild_context(&loaded.entries);
        assert_eq!(context.summary.as_deref(), Some("the summary"));
        let kept: Vec<String> = context
            .items
            .iter()
            .map(|ContextItem { entry_id, .. }| entry_id.clone())
            .collect();
        assert_eq!(kept, ["e2", "e3"]);
        SessionStore::forget("compaction-drops");
    }

    #[test]
    fn a_forgotten_or_unknown_history_cannot_resume() {
        let _store = start("history-forgotten");
        SessionStore::forget("history-forgotten");
        assert!(matches!(
            SessionStore::resume("history-forgotten", None),
            Err(SessionStoreError::Missing(_))
        ));
        assert!(matches!(
            SessionStore::resume("never-started", None),
            Err(SessionStoreError::Missing(_))
        ));
    }
}
