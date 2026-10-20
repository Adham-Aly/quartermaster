//! Your unsent message in a prompt field: its text, the cursor and the selection, kept together.
//!
//! A `Draft` is plain data with no screen geometry and no mouse or drag state (those live in
//! `View` and `Pointer`), so one draft can be swapped between fields, kept per session, and later
//! shared or saved as it is.

/// The text of a message being written, with its cursor and selection as byte offsets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    pub(crate) text: String,
    /// The cursor as a byte offset into `text` (on a char boundary); `None` is the end, where
    /// typing appends.
    pub(crate) cursor: Option<usize>,
    /// The other end of the selection, as a byte offset (on a char boundary): the text between it
    /// and the cursor is selected. `None` (or the cursor itself) is no selection.
    pub(crate) anchor: Option<usize>,
}

impl Draft {
    /// A draft holding `text`, the cursor at its end and nothing selected.
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cursor: None,
            anchor: None,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether `at` is a valid offset into the text.
    fn valid(&self, at: usize) -> bool {
        at <= self.text.len() && self.text.is_char_boundary(at)
    }

    /// The cursor as a byte offset (the end when unset or no longer valid).
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
            .filter(|&at| self.valid(at))
            .unwrap_or(self.text.len())
    }

    /// The selected bytes, start before end, when there are any.
    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor.filter(|&at| self.valid(at))?;
        let at = self.cursor();
        (anchor != at).then(|| (anchor.min(at), anchor.max(at)))
    }

    /// The selected text, when there is any.
    pub(crate) fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(from, to)| &self.text[from..to])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selection_runs_between_anchor_and_cursor_either_way() {
        let mut draft = Draft::new("hello world");
        assert_eq!(draft.selection(), None);
        draft.anchor = Some(6);
        assert_eq!(draft.selected_text(), Some("world"));
        draft.cursor = Some(2);
        assert_eq!(draft.selection(), Some((2, 6)));
        draft.anchor = Some(2);
        assert_eq!(draft.selection(), None);
    }

    #[test]
    fn offsets_off_a_char_boundary_are_ignored() {
        let mut draft = Draft::new("日本");
        draft.cursor = Some(1);
        draft.anchor = Some(4);
        assert_eq!(draft.cursor(), "日本".len());
        assert_eq!(draft.selection(), None);
    }
}
