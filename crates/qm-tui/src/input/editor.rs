//! Editing your message: the cursor, words and lines.
//!
//! The cursor is a byte offset into the text, always on a char boundary. The functions here are
//! pure, so they can be checked on multi-byte text without a terminal; `App` keeps the text, the
//! cursor and the selection together in `App::draft` (`input::draft`).

use crate::app::App;
use crate::hit::Pane;
use crate::input::draft::Draft;
use crate::input::field;
use crate::text;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qm_types::api::{AgentRef, Command, HeldAnswer, HeldChoice};
use std::time::Instant;

/// The char boundary before `at` (or 0).
pub(crate) fn prev_char(s: &str, at: usize) -> usize {
    s[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

/// The char boundary after `at` (or the end).
pub(crate) fn next_char(s: &str, at: usize) -> usize {
    s[at..]
        .chars()
        .next()
        .map_or(s.len(), |c| at + c.len_utf8())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start of the word before `at`: skips non-word chars, then word chars.
pub(crate) fn word_left(s: &str, at: usize) -> usize {
    let mut chars = s[..at].char_indices().rev().peekable();
    while chars.next_if(|&(_, c)| !is_word(c)).is_some() {}
    let mut start = chars.peek().map_or(0, |&(i, c)| i + c.len_utf8());
    while let Some((i, _)) = chars.next_if(|&(_, c)| is_word(c)) {
        start = i;
    }
    start
}

/// The end of the word after `at`: skips non-word chars, then word chars.
pub(crate) fn word_right(s: &str, at: usize) -> usize {
    let mut end = at;
    let mut chars = s[at..].chars().peekable();
    while let Some(c) = chars.next_if(|&c| !is_word(c)) {
        end += c.len_utf8();
    }
    while let Some(c) = chars.next_if(|&c| is_word(c)) {
        end += c.len_utf8();
    }
    end
}

/// The start of the line holding `at`.
pub(crate) fn line_start(s: &str, at: usize) -> usize {
    s[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The end of the line holding `at` (before its `\n`).
pub(crate) fn line_end(s: &str, at: usize) -> usize {
    s[at..].find('\n').map_or(s.len(), |i| at + i)
}

/// The offset on the line starting at `start` that is closest to `column` cells in.
fn at_column(s: &str, start: usize, column: usize) -> usize {
    let end = line_end(s, start);
    let mut used = 0;
    for (i, c) in s[start..end].char_indices() {
        let w = text::width(c.encode_utf8(&mut [0; 4]));
        if used + w > column {
            return start + i;
        }
        used += w;
    }
    end
}

/// The cursor one line up, at the same column where the line is long enough; `at` itself on the
/// first line.
pub(crate) fn line_up(s: &str, at: usize) -> usize {
    let start = line_start(s, at);
    if start == 0 {
        return at;
    }
    let column = text::width(&s[start..at]);
    at_column(s, line_start(s, start - 1), column)
}

/// The cursor one line down, at the same column where the line is long enough; `at` itself on
/// the last line.
pub(crate) fn line_down(s: &str, at: usize) -> usize {
    let end = line_end(s, at);
    if end == s.len() {
        return at;
    }
    let column = text::width(&s[line_start(s, at)..at]);
    at_column(s, end + 1, column)
}

impl App {
    /// The cursor in your message, as a byte offset.
    pub(crate) fn cursor(&self) -> usize {
        self.draft.cursor()
    }

    /// The selected bytes of your message, start before end, when there are any.
    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        self.draft.selection()
    }

    /// Moves the cursor to `at`, keeping the anchor where it is.
    fn move_cursor(&mut self, at: usize) {
        let value = (at < self.draft.text.len()).then_some(at);
        self.change(|app| &mut app.draft.cursor, value);
    }

    /// Moves the cursor to `at` and drops the selection.
    pub(crate) fn set_cursor(&mut self, at: usize) {
        self.change(|app| &mut app.draft.anchor, None);
        self.move_cursor(at);
    }

    /// Moves the cursor to `at`, selecting from where the selection started (or from the cursor).
    pub(crate) fn select_to(&mut self, at: usize) {
        if self.draft.anchor.is_none() {
            self.draft.anchor = Some(self.cursor());
        }
        self.move_cursor(at);
    }

    /// Starts a selection at the cursor (a press in the field, before a drag).
    pub(crate) fn anchor_at_cursor(&mut self) {
        let at = self.cursor();
        self.change(|app| &mut app.draft.anchor, Some(at));
    }

    /// Drops an empty selection.
    pub(crate) fn drop_empty_selection(&mut self) {
        if self.selection().is_none() {
            self.change(|app| &mut app.draft.anchor, None);
        }
    }

    /// Replaces your message with `text`, the cursor at its end and nothing selected.
    pub(crate) fn set_input(&mut self, text: String) {
        self.draft = Draft::new(text);
        self.redraw();
    }

    /// Takes your message's text out of the field, leaving it empty.
    pub(crate) fn take_input(&mut self) -> String {
        let text = std::mem::take(&mut self.draft).text;
        self.redraw();
        text
    }

    /// Inserts `s` at the cursor, in place of the selection if there is one, and moves past it.
    pub(crate) fn insert(&mut self, s: &str) {
        let (from, to) = self.selection().unwrap_or((self.cursor(), self.cursor()));
        self.draft.text.replace_range(from..to, s);
        self.set_cursor(from + s.len());
        self.redraw();
    }

    /// Deletes `from..to`, or the selection instead when there is one.
    fn delete(&mut self, from: usize, to: usize) {
        let (from, to) = self.selection().unwrap_or((from, to));
        if from < to {
            self.draft.text.replace_range(from..to, "");
            self.set_cursor(from);
            self.redraw();
        }
    }

    /// The cursor one row up or down in the field as last drawn (by line before the first frame).
    fn row_step(&self, up: bool) -> usize {
        let s = &self.draft.text;
        let at = self.cursor();
        let Some(room) = self.drawn.field.as_ref().map(|f| f.room) else {
            return if up { line_up(s, at) } else { line_down(s, at) };
        };
        let rows = field::wrap(s, room);
        if up {
            field::row_up(s, &rows, at)
        } else {
            field::row_down(s, &rows, at)
        }
    }

    /// `Ctrl`+`C` on a field holding text: clears it (another on the empty field starts quitting).
    /// Returns whether it did.
    pub(crate) fn clear_input_on_ctrl_c(&mut self) -> bool {
        let field_showing = match self.screen {
            crate::router::Screen::Bridge => true,
            crate::router::Screen::Session(_) => self.session_open(),
            _ => false,
        };
        if !field_showing
            || self.focus != Pane::Input
            || self.draft.text.is_empty()
            || self.view.menu.is_some()
            || !matches!(
                self.quit,
                crate::app::Quit::Idle | crate::app::Quit::Armed { .. }
            )
        {
            return false;
        }
        self.set_input(String::new());
        self.change(|app| &mut app.quit, crate::app::Quit::Idle);
        true
    }

    /// Sends your message, or in guidance mode answers the card with your words, or in location
    /// mode sends the folder for a project proposal.
    pub(crate) fn send_input(&mut self) {
        if self.draft.text.trim().is_empty() {
            return;
        }
        // In a worker's session: direct intervention, its own draft.
        if let crate::router::Screen::Session(agent) = self.screen {
            if !self.session_open() {
                return;
            }
            let images = self.draft.images_in_text();
            let text = self.take_input();
            self.set_session_scroll(0);
            self.command(Command::Send {
                to: agent,
                text,
                images,
            });
            return;
        }
        // Images go only with a message; guidance and a folder take none.
        let images = self.draft.images_in_text();
        let text = self.take_input();
        let command = if let Some(held) = self.view.guidance_for.take() {
            Command::AnswerHeld {
                held,
                answer: HeldAnswer::Choose {
                    choice: HeldChoice::RetryWithGuidance,
                    words: Some(text),
                },
            }
        } else if let Some(held) = self.view.location_for.take() {
            Command::ChangeHeldLocation {
                held,
                location: text.trim().to_owned(),
            }
        } else {
            self.scroll_from_bottom = 0;
            Command::Send {
                to: AgentRef::Quartermaster,
                text,
                images,
            }
        };
        self.command(command);
    }

    /// Pasted text goes into the input box at the cursor, with line endings made `\n`.
    pub(crate) fn on_paste(&mut self, pasted: &str) {
        // An open popup takes the paste: Settings into its text field, Help nowhere.
        if let Some(popup) = self.view.popup {
            if popup == crate::popups::Popup::Settings && self.view.menu.is_none() {
                self.settings_paste(pasted);
            }
            return;
        }
        if self.screen == crate::router::Screen::Setup {
            self.setup_paste(pasted);
            return;
        }

        if let crate::router::Screen::Session(_) = self.screen {
            if self.session_open()
                && self.view.menu.is_none()
                && !matches!(self.quit, crate::app::Quit::Confirm { .. })
            {
                self.focus_pane(Pane::Input);
                self.insert(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
            }
            return;
        }
        if self.screen != crate::router::Screen::Bridge
            || matches!(self.quit, crate::app::Quit::Confirm { .. })
            || self.view.menu.is_some()
        {
            return;
        }
        self.focus_pane(Pane::Input);
        self.insert(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
    }

    /// A key while the input box has focus. Shift with a movement key selects as it moves;
    /// with a selection, typing replaces it and every delete key removes it.
    pub(crate) fn on_editor_key(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let command = key.modifiers.contains(KeyModifiers::SUPER);
        let s = &self.draft.text;
        let at = self.cursor();
        let selection = self.selection();
        // Where a movement key takes the cursor.
        let to = match key.code {
            // ⌘← ⌘→ where the terminal passes them on; Terminal and iTerm2 send ⌥← and ⌥→ as
            // Esc b and Esc f by default.
            KeyCode::Left if command => Some(line_start(s, at)),
            KeyCode::Right if command => Some(line_end(s, at)),
            KeyCode::Left if alt || ctrl => Some(word_left(s, at)),
            KeyCode::Right if alt || ctrl => Some(word_right(s, at)),
            KeyCode::Char('b') if alt => Some(word_left(s, at)),
            KeyCode::Char('f') if alt => Some(word_right(s, at)),
            // Without shift, ← and → first collapse a selection to its edge.
            KeyCode::Left if !shift && selection.is_some() => selection.map(|(from, _)| from),
            KeyCode::Right if !shift && selection.is_some() => selection.map(|(_, to)| to),
            KeyCode::Left => Some(prev_char(s, at)),
            KeyCode::Char('b' | 'B') if ctrl => Some(prev_char(s, at)),
            KeyCode::Right => Some(next_char(s, at)),
            KeyCode::Char('f' | 'F') if ctrl => Some(next_char(s, at)),
            KeyCode::Up if command => Some(0),
            KeyCode::Down if command => Some(s.len()),
            KeyCode::Home if ctrl => Some(0),
            KeyCode::End if ctrl => Some(s.len()),
            KeyCode::Home => Some(line_start(s, at)),
            KeyCode::Char('a' | 'A') if ctrl => Some(line_start(s, at)),
            KeyCode::End => Some(line_end(s, at)),
            KeyCode::Char('e' | 'E') if ctrl => Some(line_end(s, at)),
            KeyCode::Up => Some(self.row_step(true)),
            KeyCode::Char('p' | 'P') if ctrl => Some(self.row_step(true)),
            KeyCode::Down => Some(self.row_step(false)),
            KeyCode::Char('n' | 'N') if ctrl => Some(self.row_step(false)),
            _ => None,
        };
        if let Some(to) = to {
            let selects = shift && !matches!(key.code, KeyCode::Char(_));
            if selects {
                self.select_to(to);
            } else {
                self.set_cursor(to);
            }
            return;
        }
        match key.code {
            KeyCode::Enter if alt || shift => self.insert("\n"),
            KeyCode::Char('j' | 'J') if ctrl => self.insert("\n"),
            KeyCode::Enter => self.send_input(),
            // An image from the clipboard; ⌘V stays the terminal's text paste.
            KeyCode::Char('v' | 'V') if ctrl => self.paste_image(now),
            KeyCode::Backspace if command => self.delete(line_start(s, at), at),
            KeyCode::Backspace if alt || ctrl => self.delete(word_left(s, at), at),
            KeyCode::Char('w' | 'W') if ctrl => self.delete(word_left(s, at), at),
            KeyCode::Char('h' | 'H') if ctrl => self.delete(prev_char(s, at), at),
            // An image's placeholder goes as one piece.
            KeyCode::Backspace => {
                let from = self.draft.placeholder_before(at);
                self.delete(from.unwrap_or_else(|| prev_char(s, at)), at);
            }
            KeyCode::Delete if alt || ctrl => self.delete(at, word_right(s, at)),
            KeyCode::Char('d') if alt => self.delete(at, word_right(s, at)),
            KeyCode::Char('d' | 'D') if ctrl => self.delete(at, next_char(s, at)),
            KeyCode::Delete => {
                let to = self.draft.placeholder_after(at);
                self.delete(at, to.unwrap_or_else(|| next_char(s, at)));
            }
            KeyCode::Char('u' | 'U') if ctrl => self.delete(line_start(s, at), at),
            // At a line's end, Ctrl+K joins the next line, as in Emacs.
            KeyCode::Char('k' | 'K') if ctrl => {
                let end = line_end(s, at);
                let end = if end == at { next_char(s, at) } else { end };
                self.delete(at, end);
            }
            KeyCode::Char(c) if !ctrl && !alt && !command => {
                self.insert(c.encode_utf8(&mut [0; 4]));
            }
            _ => {}
        }
    }
}
