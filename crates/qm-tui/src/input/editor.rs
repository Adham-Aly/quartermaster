//! Editing your message: the cursor, words and lines.
//!
//! The cursor is a byte offset into the text, always on a char boundary. The functions here are
//! pure, so they can be checked on multi-byte text without a terminal; `App` keeps the cursor in
//! `view.cursor`, with `None` meaning the end.

use crate::app::App;
use crate::hit::Pane;
use crate::text;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qm_types::api::{AgentRef, Command, HeldAnswer, HeldChoice};

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
    /// The cursor as a byte offset.
    pub(crate) fn cursor(&self) -> usize {
        self.view
            .cursor
            .filter(|&at| at <= self.input.len() && self.input.is_char_boundary(at))
            .unwrap_or(self.input.len())
    }

    fn set_cursor(&mut self, at: usize) {
        let value = (at < self.input.len()).then_some(at);
        self.change(|app| &mut app.view.cursor, value);
    }

    /// Inserts `s` at the cursor and moves past it.
    pub(crate) fn insert(&mut self, s: &str) {
        let at = self.cursor();
        self.input.insert_str(at, s);
        self.set_cursor(at + s.len());
        self.redraw();
    }

    fn delete(&mut self, from: usize, to: usize) {
        if from < to {
            self.input.replace_range(from..to, "");
            self.set_cursor(from);
            self.redraw();
        }
    }

    /// Sends your message, or in guidance mode answers the card with your words, or in location
    /// mode sends the folder for a project proposal.
    pub(crate) fn send_input(&mut self) {
        if self.input.trim().is_empty() {
            return;
        }
        // In a worker's session: direct intervention, its own draft.
        if let crate::router::Screen::Session(agent) = self.screen {
            if !self.session_open() {
                return;
            }
            let text = std::mem::take(&mut self.input);
            self.view.cursor = None;
            self.set_session_scroll(0);
            self.command(Command::Send { to: agent, text });
            return;
        }
        let text = std::mem::take(&mut self.input);
        self.view.cursor = None;
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

    /// A key while the input box has focus.
    pub(crate) fn on_editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let s = &self.input;
        let at = self.cursor();
        match key.code {
            KeyCode::Enter if alt || shift => self.insert("\n"),
            KeyCode::Char('j' | 'J') if ctrl => self.insert("\n"),
            KeyCode::Enter => self.send_input(),
            // Terminal and iTerm2 send ⌥← and ⌥→ as Esc b and Esc f by default.
            KeyCode::Left | KeyCode::Char('b') if alt => self.set_cursor(word_left(s, at)),
            KeyCode::Right | KeyCode::Char('f') if alt => self.set_cursor(word_right(s, at)),
            KeyCode::Left => self.set_cursor(prev_char(s, at)),
            KeyCode::Right => self.set_cursor(next_char(s, at)),
            KeyCode::Home => self.set_cursor(line_start(s, at)),
            KeyCode::Char('a' | 'A') if ctrl => self.set_cursor(line_start(s, at)),
            KeyCode::End => self.set_cursor(line_end(s, at)),
            KeyCode::Char('e' | 'E') if ctrl => self.set_cursor(line_end(s, at)),
            KeyCode::Up => self.set_cursor(line_up(s, at)),
            KeyCode::Down => self.set_cursor(line_down(s, at)),
            KeyCode::Backspace if alt => self.delete(word_left(s, at), at),
            KeyCode::Char('w' | 'W') if ctrl => self.delete(word_left(s, at), at),
            KeyCode::Char('h' | 'H') if ctrl => self.delete(prev_char(s, at), at),
            KeyCode::Backspace => self.delete(prev_char(s, at), at),
            KeyCode::Delete => self.delete(at, next_char(s, at)),
            KeyCode::Char('u' | 'U') if ctrl => self.delete(line_start(s, at), at),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
    }
}
