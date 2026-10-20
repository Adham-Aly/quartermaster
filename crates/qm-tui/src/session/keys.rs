//! The session screen's keys.
//!
//! | Key | Does |
//! |---|---|
//! | `Tab` / `⇧Tab` | focus Session ↔ Input (Input on entry) |
//! | `⏎` (Input) | send to the worker; `⇧⏎`, `⌥⏎`, `Ctrl`+`J` a new line |
//! | `Esc` | close the menu → clear the selection → back to the Bridge; never interrupts here |
//! | `i` (Session) | interrupt the worker's running turn |
//! | `e` (Session) | expand or collapse the newest line with a detail in view |
//! | ↑ ↓ `j` `k`, `PgUp` `PgDn`, `g` `G` (Session) | scroll |
//! | `?` (Session) | Help |
//!
//! `Ctrl`+`C`, `Ctrl`+`Z` and an open menu are handled before this, as on every screen.

use super::SessionHit;
use crate::app::App;
use crate::hit::{Hit, Pane};
use crate::router::Screen;
use crossterm::event::{KeyCode, KeyEvent};
use std::time::Instant;

impl App {
    /// A key on the session screen; returns whether it was this screen's (every key is, while
    /// the screen shows).
    pub(crate) fn on_session_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        let Screen::Session(_) = self.screen else {
            return false;
        };
        let _ = now;
        match key.code {
            KeyCode::Esc => {
                if self.view.selection.is_some() {
                    self.pointer.selecting = false;
                    self.change(|app| &mut app.view.selection, None);
                } else {
                    self.leave_session();
                }
                return true;
            }
            KeyCode::Tab => {
                self.cycle_focus(true);
                return true;
            }
            KeyCode::BackTab => {
                self.cycle_focus(false);
                return true;
            }
            _ => {}
        }
        if self.focus == Pane::Input {
            // A closed session takes no text.
            if self.session_open() {
                self.on_editor_key(key);
            }
            return true;
        }
        let max = self.drawn.scroll_max_of(Pane::Session);
        let page = self
            .drawn
            .hits
            .rect_of(Hit::Pane(Pane::Session))
            .map_or(1, |area| usize::from(area.height.saturating_sub(3)).max(1));
        let scroll = self.session_scroll();
        // Counted up from the newest line, so "up" adds.
        let to = match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some((scroll + 1).min(max)),
            KeyCode::Down | KeyCode::Char('j') => Some(scroll.saturating_sub(1)),
            KeyCode::PageUp => Some((scroll + page).min(max)),
            KeyCode::PageDown => Some(scroll.saturating_sub(page)),
            KeyCode::Home | KeyCode::Char('g') => Some(max),
            KeyCode::End | KeyCode::Char('G') => Some(0),
            _ => None,
        };
        if let Some(to) = to {
            self.set_session_scroll(to);
            return true;
        }
        match key.code {
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char(',') => self.show(Screen::Settings),
            KeyCode::Char('i') => self.interrupt_session(),
            KeyCode::Char('e') => self.expand_in_view(),
            KeyCode::Enter => self.focus_pane(Pane::Input),
            _ => {}
        }
        true
    }

    /// `e`: toggles the newest line with a detail whose button is in view.
    fn expand_in_view(&mut self) {
        let Some(view) = self.session_view() else {
            return;
        };
        let newest = view
            .lines
            .iter()
            .rev()
            .filter(|line| line.detail.is_some())
            .map(|line| line.id)
            .find(|&id| {
                self.drawn
                    .hits
                    .rect_of(Hit::Session(SessionHit::Expand(id)))
                    .is_some()
            });
        if let Some(id) = newest {
            self.toggle_expanded(id);
        }
    }
}
