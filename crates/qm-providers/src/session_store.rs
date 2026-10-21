//! Provider session history, held in memory.
//!
//! Each session's entries live in one process-wide map, keyed by its session ID (a UUID v7). Nothing
//! is written to disk: after a restart a session can't be resumed (the core sees `ResumeFailed`
//! and starts a fresh one), and an effort change within a run still finds its history here.
//!
//! The first entry is the header; every entry after it is a message, a compaction, a model change
//! or a turn end:
//!
//! ```text
//! Session     { v, id, at, provider, model, effort }
//! Message     { id: "e1", at, message: { role: "user", … } }
//! Message     { id: "e2", at, message: { role: "assistant", … } }
//! Compaction  { id: "e9", at, summary, first_kept: "e5", tokens_before, tokens_after }
//! ModelChange { id: "e10", at, model }
//! TurnEnd     { id: "e11", at, outcome: "completed" }
//! ```
//!
//! Assistant messages are recorded once complete; aborted and failed ones too, marked, and skipped
//! on replay. A compaction drops the entries before its first kept message, so the map holds the
//! header and what the next turn can see. The context is rebuilt from the latest compaction
//! ([`rebuild_context`]).

use crate::compaction::{Context, ContextItem};
use crate::transcript::Message;
use jiff::Timestamp;
use qm_types::domain::{Effort, Provider};
use qm_types::ids::ModelId;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

/// The format version in the header.
pub const FORMAT_VERSION: u32 = 1;

/// One entry of a session's history.
#[derive(Clone, Debug, PartialEq)]
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
    #[error("the session {0} has no history in this run (it was forgotten or never started)")]
    Missing(String),
}

/// Every session's entries in this process, by session ID.
static SESSIONS: LazyLock<Mutex<HashMap<String, Vec<Entry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sessions() -> MutexGuard<'static, HashMap<String, Vec<Entry>>> {
    // A panic elsewhere can't corrupt a `Vec` push, so a poisoned lock still holds good history.
    SESSIONS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A handle on one session's history, appending.
#[derive(Debug)]
pub struct SessionStore {
    session_id: String,
    next_entry: u64,
}

/// A session's history read back.
#[derive(Debug)]
pub struct LoadedSession {
    pub store: SessionStore,
    pub entries: Vec<Entry>,
}

impl SessionStore {
    /// Starts a session's history with its header.
    pub fn create(
        session_id: &str,
        at: Timestamp,
        provider: Provider,
        model: ModelId,
        effort: Effort,
    ) -> Self {
        let header = Entry::Session {
            v: FORMAT_VERSION,
            id: session_id.to_owned(),
            at,
            provider,
            model,
            effort,
        };
        sessions().insert(session_id.to_owned(), vec![header]);
        Self {
            session_id: session_id.to_owned(),
            next_entry: 1,
        }
    }

    /// Reads a session's history back, to continue it.
    pub fn resume(session_id: &str) -> Result<LoadedSession, SessionStoreError> {
        let entries = sessions()
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionStoreError::Missing(session_id.to_owned()))?;
        let next_entry = entries
            .iter()
            .filter_map(entry_number)
            .max()
            .map_or(1, |n| n + 1);
        Ok(LoadedSession {
            store: Self {
                session_id: session_id.to_owned(),
                next_entry,
            },
            entries,
        })
    }

    /// Drops a session's history. A later resume of it fails with [`SessionStoreError::Missing`].
    pub fn forget(session_id: &str) {
        sessions().remove(session_id);
    }

    /// The next entry's ID (`e<n>`), reserved.
    pub fn next_id(&mut self) -> String {
        let id = format!("e{}", self.next_entry);
        self.next_entry += 1;
        id
    }

    /// Appends one entry. A compaction then drops the entries before its first kept message.
    pub fn write(&mut self, entry: &Entry) -> Result<(), SessionStoreError> {
        let mut all = sessions();
        let entries = all
            .get_mut(&self.session_id)
            .ok_or_else(|| SessionStoreError::Missing(self.session_id.clone()))?;
        entries.push(entry.clone());
        if let Entry::Compaction { first_kept, .. } = entry {
            keep_from(entries, first_kept);
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
        SessionStore::create(id, at, Provider::ChatGpt, model, Effort::Medium)
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

        let loaded = SessionStore::resume("history-resumes").expect("the history is held");
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

        let loaded = SessionStore::resume("compaction-drops").expect("the history is held");
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
            SessionStore::resume("history-forgotten"),
            Err(SessionStoreError::Missing(_))
        ));
        assert!(matches!(
            SessionStore::resume("never-started"),
            Err(SessionStoreError::Missing(_))
        ));
    }
}
