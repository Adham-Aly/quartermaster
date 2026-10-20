//! Your unsent message in a prompt field: its text, the cursor, the selection and the images
//! pasted into it, kept together.
//!
//! A `Draft` is plain data with no screen geometry and no mouse or drag state (those live in
//! `View` and `Pointer`), so one draft can be swapped between fields, kept per session, and later
//! shared or saved as it is.

use qm_types::api::PastedImage;

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
    /// The images pasted with `Ctrl`+`V`, each shown in the text as its `[Image #N]`; one whose
    /// placeholder is gone from the text is dropped.
    pub(crate) images: Vec<PastedImage>,
}

impl Draft {
    /// A draft holding `text`, the cursor at its end and nothing selected.
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cursor: None,
            anchor: None,
            images: Vec::new(),
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

    /// Where an image's placeholder ending at `at` starts, so `⌫` takes it as one piece.
    pub(crate) fn placeholder_before(&self, at: usize) -> Option<usize> {
        let before = self.text.get(..at)?;
        self.images
            .iter()
            .map(PastedImage::placeholder)
            .find(|p| before.ends_with(p.as_str()))
            .map(|p| at - p.len())
    }

    /// Where an image's placeholder starting at `at` ends, so `delete` takes it as one piece.
    pub(crate) fn placeholder_after(&self, at: usize) -> Option<usize> {
        let after = self.text.get(at..)?;
        self.images
            .iter()
            .map(PastedImage::placeholder)
            .find(|p| after.starts_with(p.as_str()))
            .map(|p| at + p.len())
    }

    /// Forgets the images whose placeholders are gone from the text.
    pub(crate) fn prune_images(&mut self) {
        let text = &self.text;
        self.images
            .retain(|image| text.contains(&image.placeholder()));
    }

    /// The number for the next pasted image: one past the highest still in the text.
    pub(crate) fn next_image_number(&self) -> u32 {
        self.images
            .iter()
            .filter(|image| self.text.contains(&image.placeholder()))
            .map(|image| image.number)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// The images whose placeholders are still in the text: what goes with the message.
    pub(crate) fn images_in_text(&self) -> Vec<PastedImage> {
        self.images
            .iter()
            .filter(|image| self.text.contains(&image.placeholder()))
            .cloned()
            .collect()
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

    fn png(number: u32) -> PastedImage {
        PastedImage::new(
            number,
            qm_types::api::ImageFormat::Png,
            b"\x89PNG\r\n\x1a\n".to_vec(),
        )
    }

    #[test]
    fn an_image_placeholder_deletes_as_one_piece() {
        let mut draft = Draft::new("see [Image #1] and [Image #2]");
        draft.images = vec![png(1), png(2)];
        assert_eq!(draft.placeholder_before(14), Some(4));
        assert_eq!(draft.placeholder_before(13), None);
        assert_eq!(draft.placeholder_after(19), Some(29));
        assert_eq!(draft.placeholder_after(4), Some(14));
        // Only attached images count: a typed placeholder is plain text.
        draft.images = vec![png(2)];
        assert_eq!(draft.placeholder_before(14), None);
    }

    #[test]
    fn only_images_still_in_the_text_go_and_numbering_follows_them() {
        let mut draft = Draft::new("first [Image #1], gone, third [Image #3]");
        draft.images = vec![png(1), png(2), png(3)];
        let sent: Vec<u32> = draft.images_in_text().iter().map(|i| i.number).collect();
        assert_eq!(sent, vec![1, 3]);
        assert_eq!(draft.next_image_number(), 4);
        draft.text = String::from("first [Image #1]");
        assert_eq!(draft.next_image_number(), 2);
        draft.prune_images();
        assert_eq!(draft.images.len(), 1);
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
