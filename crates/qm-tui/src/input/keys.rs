//! The key map, complete.

use crate::app::{App, Quit};
use crate::hit::{Hit, Pane};
use crate::popups::MenuEntry;
use crate::router::Screen;
use crate::view::MenuTarget;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

/// A list pane's selection after a movement key, given `rows` rows and the current selection.
/// `None` when the key doesn't move a list.
pub(crate) fn list_move(code: KeyCode, current: Option<usize>, rows: usize) -> Option<usize> {
    let last = rows.checked_sub(1)?;
    let at = match code {
        KeyCode::Up | KeyCode::Char('k') => current.map_or(0, |i| i.saturating_sub(1)),
        KeyCode::Down | KeyCode::Char('j') => current.map_or(0, |i| (i + 1).min(last)),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return None,
    };
    Some(at.min(last))
}

/// A scroll offset moved by `delta` lines, clamped to `0..=max`.
fn scrolled(current: usize, delta: isize, max: usize) -> usize {
    current.saturating_add_signed(delta).min(max)
}

impl App {
    /// Help's keys: scrolling.
    pub(crate) fn on_help_key(&mut self, code: KeyCode) {
        let max = self.drawn.scroll_max_of(Pane::Help);
        let page = self
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == Pane::Help)
            .map_or(1, |(area, _)| {
                usize::from(area.height.saturating_sub(3)).max(1)
            });
        let current = self.view.help_scroll;
        let to = match code {
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (current + 1).min(max),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => (current + page).min(max),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => max,
            _ => return,
        };
        self.change(|app| &mut app.view.help_scroll, to);
    }

    /// Applies a key press.
    pub(crate) fn on_key(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'C')) {
            // Your message first, as in Claude Code: Ctrl+C clears it; on an empty field it quits.
            if !self.clear_input_on_ctrl_c() {
                self.on_ctrl_c(now);
            }
            return;
        }
        if let Quit::Confirm { .. } = self.quit {
            match key.code {
                KeyCode::Enter => self.confirm_quit(),
                KeyCode::Esc => self.cancel_quit(),
                _ => {}
            }
            return;
        }
        // Any other key ends a pending first Ctrl+C.
        if let Quit::Armed { .. } = self.quit {
            self.change(|app| &mut app.quit, Quit::Idle);
        }
        if ctrl && matches!(key.code, KeyCode::Char('z' | 'Z')) {
            self.suspend = true;
            return;
        }
        // `Ctrl`+`O` opens the top bar's menu from anywhere, even while typing.
        if ctrl && matches!(key.code, KeyCode::Char('o' | 'O')) {
            self.toggle_main_menu();
            return;
        }
        if self.view.menu.is_some() {
            self.on_menu_key(key.code, now);
            return;
        }
        // An open popup takes every other key.
        if let Some(popup) = self.view.popup {
            self.on_popup_key(popup, key, now);
            return;
        }
        if self.screen == Screen::Setup && self.on_setup_key(key, now) {
            return;
        }
        if self.screen == Screen::Diff && self.on_diff_key(key, now) {
            return;
        }

        if let Screen::Session(_) = self.screen
            && self.on_session_key(key, now)
        {
            return;
        }
        if key.code == KeyCode::Esc {
            self.escape();
            return;
        }
        if self.screen != Screen::Bridge {
            if key.code == KeyCode::Char('?') {
                self.open_entry(MenuEntry::Help);
            } else if key.code == KeyCode::Char(',') {
                self.open_entry(MenuEntry::Settings);
            }
            return;
        }
        match key.code {
            KeyCode::Tab => {
                self.show_selection();
                return self.cycle_focus(true);
            }
            KeyCode::BackTab => {
                self.show_selection();
                return self.cycle_focus(false);
            }
            _ => {}
        }
        if self.focus == Pane::Input {
            self.on_editor_key(key, now);
            return;
        }
        if key.code == KeyCode::Char('?') {
            self.open_entry(MenuEntry::Help);
            return;
        }
        // `,` opens Settings, as in most Mac apps.
        if key.code == KeyCode::Char(',') {
            self.open_entry(MenuEntry::Settings);
            return;
        }
        match self.focus {
            Pane::Projects => self.on_projects_key(key.code),
            Pane::Crew => self.on_crew_key(key.code, now),
            Pane::Conversation => self.on_conversation_key(key.code, now),
            Pane::Task => self.on_task_key(key.code, now),
            // Other screens' and popups' panes: their keys are theirs.
            Pane::Session | Pane::Settings | Pane::Help | Pane::Input => {}
        }
    }

    fn on_menu_key(&mut self, code: KeyCode, now: Instant) {
        let Some(menu) = &self.view.menu else { return };
        let len = menu.items.len();
        let cursor = menu.cursor;
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.set_menu_cursor(cursor.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.set_menu_cursor((cursor + 1).min(len.saturating_sub(1)));
            }
            KeyCode::Enter => self.choose_menu_item(cursor, now),
            KeyCode::Esc => self.change(|app| &mut app.view.menu, None),
            _ => {}
        }
    }

    /// The keyboard is moving through lists: selected rows show their fill again.
    pub(crate) fn show_selection(&mut self) {
        self.change(|app| &mut app.view.pointer_picked, false);
    }

    fn on_projects_key(&mut self, code: KeyCode) {
        self.show_selection();
        let rows = self.snapshot.projects.len();
        if code == KeyCode::Char(' ') {
            if let Some(i) = self.view.selected_project.filter(|&i| i < rows) {
                self.open_menu_for_row(MenuTarget::Project(i), Hit::Project(i));
            }
            return;
        }
        if let Some(at) = list_move(code, self.view.selected_project, rows) {
            self.change(|app| &mut app.view.selected_project, Some(at));
            self.keep_row_visible(Pane::Projects, at);
        }
    }

    fn on_crew_key(&mut self, code: KeyCode, now: Instant) {
        self.show_selection();
        let rows: Vec<_> = self.snapshot.crew.iter().map(|row| row.agent).collect();
        let current = self
            .view
            .selected_crew
            .and_then(|agent| rows.iter().position(|&a| a == agent));
        match code {
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(at) = current {
                    self.open_crew(rows[at], true, now);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(at) = current {
                    self.open_menu_for_row(MenuTarget::Crew(rows[at]), Hit::Crew(rows[at]));
                }
            }
            _ => {
                if let Some(at) = list_move(code, current, rows.len()) {
                    self.select_crew(rows[at]);
                    self.keep_row_visible(Pane::Crew, at);
                }
            }
        }
    }

    /// Scrolls a list pane so row `at` shows, using the last frame's box for its height.
    fn keep_row_visible(&mut self, pane: Pane, at: usize) {
        let Some(area) = self.drawn.hits.rect_of(Hit::Pane(pane)) else {
            return;
        };
        let visible = usize::from(area.height.saturating_sub(2)).max(1);
        let max = self.drawn.scroll_max_of(pane);
        let scroll = match pane {
            Pane::Projects => &mut self.view.projects_scroll,
            Pane::Crew => &mut self.view.crew_scroll,
            _ => return,
        };
        let mut to = *scroll;
        if at < to {
            to = at;
        } else if at >= to + visible {
            to = at + 1 - visible;
        }
        let to = to.min(max);
        if *scroll != to {
            *scroll = to;
            self.redraw();
        }
    }

    /// The height of a page in `pane`, from the last frame: its inner height less one line of
    /// overlap.
    fn page(&self, pane: Pane) -> isize {
        let rows = self
            .drawn
            .hits
            .rect_of(Hit::Pane(pane))
            .map_or(1, |area| area.height.saturating_sub(3).max(1));
        isize::from(i16::try_from(rows).unwrap_or(i16::MAX))
    }

    fn on_conversation_key(&mut self, code: KeyCode, now: Instant) {
        let max = self.drawn.conversation_scroll_max;
        let page = self.page(Pane::Conversation);
        // `scroll_from_bottom` counts up from the newest line, so "up" adds.
        let delta = match code {
            KeyCode::Up | KeyCode::Char('k') => Some(1),
            KeyCode::Down | KeyCode::Char('j') => Some(-1),
            KeyCode::PageUp => Some(page),
            KeyCode::PageDown => Some(-page),
            KeyCode::Home | KeyCode::Char('g') => Some(isize::MAX),
            KeyCode::End | KeyCode::Char('G') => Some(isize::MIN),
            _ => None,
        };
        if let Some(delta) = delta {
            let to = scrolled(self.scroll_from_bottom, delta, max);
            self.change(|app| &mut app.scroll_from_bottom, to);
            return;
        }
        match code {
            KeyCode::Char('[') => self.step_card(false),
            KeyCode::Char(']') => self.step_card(true),
            KeyCode::Char(c) if c.is_ascii_alphanumeric() => {
                self.press_card_key(c, now);
            }
            _ => {}
        }
    }

    fn on_task_key(&mut self, code: KeyCode, now: Instant) {
        let max = self.drawn.scroll_max_of(Pane::Task);
        let page = self.page(Pane::Task);
        let delta = match code {
            KeyCode::Up | KeyCode::Char('k') => Some(-1),
            KeyCode::Down | KeyCode::Char('j') => Some(1),
            KeyCode::PageUp => Some(-page),
            KeyCode::PageDown => Some(page),
            KeyCode::Home | KeyCode::Char('g') => Some(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => Some(isize::MAX),
            _ => None,
        };
        if let Some(delta) = delta {
            let to = scrolled(self.view.task_scroll, delta, max);
            self.change(|app| &mut app.view.task_scroll, to);
            return;
        }
        match code {
            KeyCode::Enter => self.open_session(now),
            KeyCode::Char('s') => self.stop_selected(),
            _ => {}
        }
    }

    /// The task pane's Open session (button or `⏎`).
    pub(crate) fn open_session(&mut self, now: Instant) {
        if let Some(agent) = self.view.selected_crew {
            self.open_crew(agent, true, now);
        }
    }
}
