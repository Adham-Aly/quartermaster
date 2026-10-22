//! Your unsent drafts, held in memory by the core for as long as the server runs, never on disk.
//! The UI sends each draft as it changes ([`Command::SaveDraft`]) and every snapshot carries them
//! all, so a UI that starts again gets its drafts back. [`DraftsPart`] is that store.

use crate::api::{ApiPart, Handled};
use qm_types::BoxFuture;
use qm_types::api::{Command, SavedDraft, Snapshot};
use std::sync::{Mutex, PoisonError};

/// The most drafts kept at once: one per prompt, so this is far more than anyone writes.
pub const MAX_DRAFTS: usize = 256;

/// The drafts, one per prompt.
#[derive(Debug, Default)]
pub struct DraftsPart {
    drafts: Mutex<Vec<SavedDraft>>,
}

impl DraftsPart {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keeps `draft` in place of the one for its prompt. An empty draft forgets that prompt's
    /// draft; a new prompt beyond [`MAX_DRAFTS`] isn't kept.
    fn save(&self, draft: &SavedDraft) {
        let mut drafts = self.drafts.lock().unwrap_or_else(PoisonError::into_inner);
        drafts.retain(|kept| kept.target != draft.target);
        if !draft.is_empty() && drafts.len() < MAX_DRAFTS {
            drafts.push(draft.clone());
        }
    }
}

impl ApiPart for DraftsPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        let Command::SaveDraft { draft } = command else {
            return None;
        };
        Some(Box::pin(async move {
            self.save(draft);
            Handled::Unchanged
        }))
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let drafts = self.drafts.lock().unwrap_or_else(PoisonError::into_inner);
            snapshot.drafts.clone_from(&drafts);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::api::{AgentRef, DraftTarget};

    fn session(agent: AgentRef, text: &str) -> SavedDraft {
        SavedDraft::new(DraftTarget::Session { agent }, text)
    }

    async fn send(part: &DraftsPart, draft: SavedDraft) {
        let command = Command::SaveDraft { draft };
        let handled = part.handle(&command).expect("a draft command").await;
        assert_eq!(handled, Handled::Unchanged, "a draft never needs a new snapshot");
    }

    async fn filled(part: &DraftsPart) -> Vec<SavedDraft> {
        let mut snapshot = Snapshot::default();
        part.fill(&mut snapshot).await;
        snapshot.drafts
    }

    #[tokio::test]
    async fn a_draft_is_held_for_each_prompt_and_replaced_in_place() {
        let part = DraftsPart::new();
        send(&part, SavedDraft::new(DraftTarget::Bridge, "first")).await;
        send(&part, session(AgentRef::Quartermaster, "other")).await;
        send(&part, SavedDraft::new(DraftTarget::Bridge, "second")).await;

        let texts: Vec<String> = filled(&part).await.into_iter().map(|d| d.text).collect();
        assert_eq!(texts, vec!["other".to_owned(), "second".to_owned()]);
    }

    #[tokio::test]
    async fn an_empty_draft_forgets_its_prompt() {
        let part = DraftsPart::new();
        send(&part, SavedDraft::new(DraftTarget::Bridge, "words")).await;
        send(&part, SavedDraft::new(DraftTarget::Bridge, "")).await;

        assert_eq!(filled(&part).await, Vec::new());
    }

    #[tokio::test]
    async fn other_commands_are_not_drafts() {
        let part = DraftsPart::new();
        assert!(part.handle(&Command::RequestQuit).is_none());
    }
}
