//! Your unsent drafts, held by the core for as long as the server runs and never written to disk.
//! The UI sends each draft as it changes and gets them all back on its first snapshot, so a
//! restart of the UI loses nothing. A draft's images are not in the core: their bytes are files the
//! UI writes in the scratch drafts folder, and a draft names each by its `token`.

use super::{AgentRef, ImageFormat};
use serde::{Deserialize, Serialize};

/// Whose prompt a draft is: the Quartermaster's, or a session's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DraftTarget {
    /// The Quartermaster's prompt.
    Bridge,
    /// A session's prompt.
    Session { agent: AgentRef },
}

/// One unsent draft: its words, the cursor, the selection and the pasted images it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SavedDraft {
    pub target: DraftTarget,
    pub text: String,
    /// The cursor as a byte offset into `text` (on a char boundary); `None` is the end.
    pub cursor: Option<usize>,
    /// The other end of the selection, as a byte offset; `None` is no selection.
    pub anchor: Option<usize>,
    pub images: Vec<DraftImage>,
}

impl SavedDraft {
    /// A draft holding `text`, the cursor at its end and nothing selected, with no images.
    pub fn new(target: DraftTarget, text: impl Into<String>) -> Self {
        Self {
            target,
            text: text.into(),
            cursor: None,
            anchor: None,
            images: Vec::new(),
        }
    }

    /// Whether there are no words: an empty draft is no draft, so the core forgets it.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// A pasted image a draft names: its number in the text (the `N` of `[Image #N]`), its format,
/// its size, and the token of its file in the scratch drafts folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DraftImage {
    pub number: u32,
    pub format: ImageFormat,
    pub len: usize,
    pub token: String,
}

impl DraftImage {
    pub fn new(number: u32, format: ImageFormat, len: usize, token: impl Into<String>) -> Self {
        Self {
            number,
            format,
            len,
            token: token.into(),
        }
    }
}
