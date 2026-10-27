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
    /// The help popups' keys: scrolling their independent viewports.
    pub(crate) fn on_help_key(&mut self, code: KeyCode) {
        let kinds = self.popup() == Some(crate::popups::Popup::TaskKinds);
        let pane = if kinds { Pane::TaskKinds } else { Pane::Help };
        let max = self.term.drawn.scroll_max_of(pane);
        let page = self
            .term
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == pane)
            .map_or(1, |(area, _)| {
                usize::from(area.height.saturating_sub(3)).max(1)
            });
        let current = if kinds {
            self.view.kind_help_scroll
        } else {
            self.view.help_scroll
        };
        let to = match code {
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (current + 1).min(max),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => (current + page).min(max),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => max,
            _ => return,
        };
        if kinds {
            self.change(|app| &mut app.view.kind_help_scroll, to);
        } else {
            self.change(|app| &mut app.view.help_scroll, to);
        }
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
        if let Quit::Confirm { .. } = self.term.quit {
            match key.code {
                KeyCode::Enter => self.confirm_quit(),
                KeyCode::Esc => self.cancel_quit(),
                _ => {}
            }
            return;
        }
        // Any other key ends a pending first Ctrl+C.
        if let Quit::Armed { .. } = self.term.quit {
            self.change(|app| &mut app.term.quit, Quit::Idle);
        }
        if ctrl && matches!(key.code, KeyCode::Char('z' | 'Z')) {
            self.term.suspend = true;
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
        if let Some(popup) = self.popup() {
            self.on_popup_key(popup, key, now);
            return;
        }
        if self.screen == Screen::Setup && self.on_setup_key(key, now) {
            return;
        }
        if self.screen == Screen::Diff && self.on_diff_key(key, now) {
            return;
        }

        // The slash-command menu takes its keys (Tab included) before focus and the editor.
        if self.on_slash_key(key) {
            return;
        }
        if key.code == KeyCode::Esc {
            self.escape();
            return;
        }
        if self.screen != Screen::Bridge {
            if key.code == KeyCode::Char('?') {
                self.choose_entry(MenuEntry::Help);
            } else if key.code == KeyCode::Char(',') {
                self.choose_entry(MenuEntry::Settings);
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
        if self.on_model_setup_key(key.code) {
            return;
        }
        if self.focus == Pane::Input {
            // A closed session takes no text.
            if self.shown_session().is_none() || self.session_open() {
                self.on_editor_key(key, now);
            }
            return;
        }
        if key.code == KeyCode::Char('?') {
            self.choose_entry(MenuEntry::Help);
            return;
        }
        // `,` opens Settings, as in most Mac apps.
        if key.code == KeyCode::Char(',') {
            self.choose_entry(MenuEntry::Settings);
            return;
        }
        match self.focus {
            Pane::Projects => self.on_projects_key(key.code),
            Pane::Crew => self.on_crew_key(key.code, now),
            Pane::Conversation => self.on_conversation_key(key.code),
            Pane::Task => self.on_task_key(key.code, now),
            Pane::Session => self.on_session_key(key.code),
            Pane::Actions => {
                if !self.on_actions_key(key.code)
                    && key.code == KeyCode::Char(' ')
                    && let Some(i) = self.selected_action_index()
                {
                    let target = MenuTarget::Action(i);
                    self.open_menu(target, crate::popups::Anchor::Row { dx: 2, dy: 1 });
                }
            }
            // Popups' panes: their keys are theirs.
            Pane::Settings | Pane::Help | Pane::TaskKinds | Pane::ActionOverlay | Pane::Input => {}
        }
    }

    fn on_model_setup_key(&mut self, code: KeyCode) -> bool {
        if !self.model_gated() || self.shown_session().is_some() {
            return false;
        }
        let entry = match code {
            KeyCode::Char(',') => Some(MenuEntry::Settings),
            KeyCode::Char('?') => Some(MenuEntry::Help),
            _ => None,
        };
        if let Some(entry) = entry {
            self.choose_entry(entry);
            return true;
        }
        self.focus == Pane::Input
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
        if code == KeyCode::Char('a') {
            self.open_projects_hint();
            return;
        }
        if code == KeyCode::Char(' ') {
            if let Some(i) = self.view.selected_project.filter(|&i| i < rows) {
                self.open_menu_for_row(MenuTarget::Project(i));
            }
            return;
        }
        if code == KeyCode::Enter {
            // With nothing picked yet, Enter opens the first.
            let picked = self.view.selected_project.filter(|&i| i < rows);
            if let Some(i) = picked.or((rows > 0).then_some(0)) {
                self.change(|app| &mut app.view.selected_project, Some(i));
                self.toggle_project(i);
            }
            return;
        }
        if let Some(at) = list_move(code, self.view.selected_project, rows) {
            self.change(|app| &mut app.view.selected_project, Some(at));
            let lines = crate::widgets::projects::lines_of(self, at);
            self.keep_row_visible(Pane::Projects, lines.end - 1);
            self.keep_row_visible(Pane::Projects, lines.start);
        }
    }

    fn on_crew_key(&mut self, code: KeyCode, now: Instant) {
        self.show_selection();
        // The agents, each with its lines in the list as last drawn, in the order drawn.
        let spots = self.term.drawn.crew_spots.clone();
        let current = self
            .view
            .selected_crew
            .and_then(|agent| spots.iter().position(|(_, a)| *a == agent));
        match code {
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(at) = current {
                    self.open_crew(spots[at].1, true, now);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(at) = current {
                    self.open_menu_for_row(MenuTarget::Crew(spots[at].1));
                }
            }
            _ => {
                if let Some(at) = list_move(code, current, spots.len()) {
                    let (lines, agent) = spots[at].clone();
                    self.select_crew(agent);
                    // The last line, then the first, so a task's name above its crewmate shows.
                    self.keep_row_visible(Pane::Crew, lines.end.saturating_sub(1));
                    self.keep_row_visible(Pane::Crew, lines.start);
                }
            }
        }
    }

    /// Scrolls a list pane so row `at` shows, using the last frame's box for its height.
    pub(crate) fn keep_row_visible(&mut self, pane: Pane, at: usize) {
        let Some(area) = self.term.drawn.hits.rect_of(Hit::Pane(pane)) else {
            return;
        };
        let max = self.term.drawn.scroll_max_of(pane);
        // A Projects list that doesn't fit gives its last line to `↓ 4 more`.
        let reserved = usize::from(pane == Pane::Projects && max > 0);
        let visible = usize::from(area.height.saturating_sub(2))
            .saturating_sub(reserved)
            .max(1);
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
            .term
            .drawn
            .hits
            .rect_of(Hit::Pane(pane))
            .map_or(1, |area| area.height.saturating_sub(3).max(1));
        isize::from(i16::try_from(rows).unwrap_or(i16::MAX))
    }

    fn on_conversation_key(&mut self, code: KeyCode) {
        let max = self.term.drawn.conversation_scroll_max;
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
        }
    }

    fn on_task_key(&mut self, code: KeyCode, now: Instant) {
        let max = self.term.drawn.scroll_max_of(Pane::Task);
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
            KeyCode::Char('r') => self.open_selected_report(),
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
