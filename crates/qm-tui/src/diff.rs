//! Side-by-side diff review, opened only from the held action's Diff button.
//!
//! The core owns the parsed patch and approved head; this module owns navigation only.
//! Tab switches between the changed-file tree and code. Brackets move between files;
//! arrows/page/home/end scroll the focused pane, and left/right pan long code lines.
//! The held card's answer keys and Escape/Back retain the Actions overlay's behaviour.

mod render;
mod rows;
mod tree;

use crate::app::App;
use crate::hit::{FrameMap, Hit};
use crate::router::Screen;
use crossterm::event::{KeyCode, KeyEvent};
use qm_types::api::{ActionItem, AgentRef, Command, DiffView};
use qm_types::ids::HeldActionId;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use std::time::Instant;

/// Something on the Diff screen a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffHit {
    Back,
    Body,
    Tree,
    File(usize),
}

/// Navigation reset on opening a diff; no file selection is saved in UI preferences.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiffUi {
    pub(crate) held: Option<HeldActionId>,
    pub(crate) file: usize,
    pub(crate) scroll: usize,
    pub(crate) horizontal: usize,
    pub(crate) tree_scroll: usize,
    pub(crate) tree_focus: bool,
}

pub(crate) const HINTS: &str =
    "tab files/code · [ ] file · ↑↓ scroll · ←→ pan · a/r answer · esc back";
const WHEEL_LINES: usize = 3;

impl App {
    pub(crate) fn show_diff(&mut self, held: Option<HeldActionId>, now: Instant) {
        let Some(held) = held else {
            self.flash(crate::input::NOT_BUILT, now);
            return;
        };
        self.view.diff = DiffUi {
            held: Some(held),
            ..DiffUi::default()
        };
        self.command(Command::ShowDiff { held: Some(held) });
        self.show(Screen::Diff);
    }

    /// Escape reopens the originating Actions item only while it still waits.
    pub(crate) fn close_diff(&mut self) {
        self.view.diff = DiffUi::default();
        self.command(Command::ShowDiff { held: None });
        self.show(Screen::Bridge);
        let from = self.view.diff_from.take();
        if let Some(index) = from.and_then(|key| {
            self.snapshot
                .actions
                .iter()
                .position(|item| item.key == key)
        }) {
            self.open_action(index);
        }
    }

    /// Do not display or answer an earlier held action while the requested diff is loading.
    fn diff_view(&self) -> Option<&DiffView> {
        self.snapshot
            .diff
            .as_ref()
            .filter(|view| Some(view.held) == self.view.diff.held)
    }

    fn diff_page(&self) -> usize {
        let hit = if self.view.diff.tree_focus {
            DiffHit::Tree
        } else {
            DiffHit::Body
        };
        self.term
            .drawn
            .hits
            .rect_of(Hit::Diff(hit))
            .map_or(1, |r| usize::from(r.height.saturating_sub(1)).max(1))
    }

    fn scroll_diff_to(&mut self, to: usize) {
        let to = to.min(self.term.drawn.diff_scroll_max);
        self.change(|app| &mut app.view.diff.scroll, to);
    }

    fn select_diff_file(&mut self, index: usize) {
        let count = self.diff_view().map_or(0, |view| view.files.len());
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if self.view.diff.file != index {
            self.change(|app| &mut app.view.diff.file, index);
            self.change(|app| &mut app.view.diff.scroll, 0);
            self.change(|app| &mut app.view.diff.horizontal, 0);
            self.change(|app| &mut app.term.selection, None);
        }
        // Keep the selected leaf visible, including all of its parent folder rows.
        let entries = tree::entries(self.diff_view().map_or(&[], |v| v.files.as_slice()));
        if let Some(row) = entries.iter().position(|e| e.file == Some(index)) {
            let height = self
                .term
                .drawn
                .hits
                .rect_of(Hit::Diff(DiffHit::Tree))
                .map_or(1, |r| usize::from(r.height).max(1));
            let top = self.view.diff.tree_scroll;
            let to = top.min(row).max(row.saturating_sub(height - 1));
            self.change(|app| &mut app.view.diff.tree_scroll, to);
        }
    }

    fn diff_file_order(&self) -> Vec<usize> {
        tree::entries(self.diff_view().map_or(&[], |v| v.files.as_slice()))
            .into_iter()
            .filter_map(|entry| entry.file)
            .collect()
    }

    /// Follow the visible tree order, which may differ from git's order after a rename.
    fn select_diff_tree_file(&mut self, position: usize) {
        let order = self.diff_file_order();
        if let Some(&index) = order.get(position.min(order.len().saturating_sub(1))) {
            self.select_diff_file(index);
        }
    }

    /// The existing approval keys are resolved against the displayed held action, never a file.
    fn diff_item(&self) -> Option<&ActionItem> {
        let held = self.diff_view()?.held;
        self.snapshot
            .actions
            .iter()
            .find(|item| item.card.held == Some(held))
    }

    pub(crate) fn on_diff_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        if matches!(
            key.code,
            KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Enter
                | KeyCode::Char('[' | ']' | 'j' | 'k' | 'g' | 'G')
        ) {
            self.show_selection();
        }
        let current = self.view.diff.scroll;
        let file = self
            .diff_file_order()
            .iter()
            .position(|&i| i == self.view.diff.file)
            .unwrap_or(0);
        let tree = self.view.diff.tree_focus;
        match key.code {
            KeyCode::Esc => self.close_diff(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.change(|app| &mut app.view.diff.tree_focus, !tree);
                self.select_diff_file(self.view.diff.file);
            }
            KeyCode::Char('[') => self.select_diff_tree_file(file.saturating_sub(1)),
            KeyCode::Char(']') => self.select_diff_tree_file(file.saturating_add(1)),
            KeyCode::Up | KeyCode::Char('k') if tree => {
                self.select_diff_tree_file(file.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') if tree => {
                self.select_diff_tree_file(file.saturating_add(1));
            }
            KeyCode::PageUp if tree => {
                self.select_diff_tree_file(file.saturating_sub(self.diff_page()));
            }
            KeyCode::PageDown if tree => {
                self.select_diff_tree_file(file.saturating_add(self.diff_page()));
            }
            KeyCode::Home | KeyCode::Char('g') if tree => self.select_diff_tree_file(0),
            KeyCode::End | KeyCode::Char('G') if tree => self.select_diff_tree_file(usize::MAX),
            KeyCode::Enter if tree => self.change(|app| &mut app.view.diff.tree_focus, false),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_diff_to(current.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_diff_to(current.saturating_add(1)),
            KeyCode::PageUp => self.scroll_diff_to(current.saturating_sub(self.diff_page())),
            KeyCode::PageDown => self.scroll_diff_to(current.saturating_add(self.diff_page())),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_diff_to(0),
            KeyCode::End | KeyCode::Char('G') => self.scroll_diff_to(usize::MAX),
            KeyCode::Left => {
                let to = self.view.diff.horizontal.saturating_sub(8);
                self.change(|app| &mut app.view.diff.horizontal, to);
            }
            KeyCode::Right => {
                let to = self
                    .view
                    .diff
                    .horizontal
                    .saturating_add(8)
                    .min(self.term.drawn.diff_horizontal_max);
                self.change(|app| &mut app.view.diff.horizontal, to);
            }
            KeyCode::Char(c) => {
                let Some(item) = self.diff_item() else {
                    return false;
                };
                if item.card.answered.is_some() {
                    return false;
                }
                let Some(action) = item
                    .card
                    .buttons
                    .iter()
                    .find(|b| b.key.eq_ignore_ascii_case(&c))
                    .map(|b| b.action.clone())
                else {
                    return false;
                };
                let held = item.card.held;
                let reply_to = item.from.unwrap_or(AgentRef::Quartermaster);
                self.view.diff_from = None;
                self.close_diff();
                self.run_card_action(action, held, None, reply_to, now);
            }
            _ => return false,
        }
        true
    }

    /// Scroll only the pane under the pointer; the tree wheel does not change the selected file.
    pub(crate) fn diff_wheel(&mut self, at: Position, up: bool) {
        let hit = self.term.drawn.hits.at(at.x, at.y);
        let tree = matches!(hit, Some(Hit::Diff(DiffHit::Tree | DiffHit::File(_))));
        let body = matches!(hit, Some(Hit::Diff(DiffHit::Body)));
        if !tree && !body {
            return;
        }
        let current = if tree {
            self.view.diff.tree_scroll
        } else {
            self.view.diff.scroll
        };
        let to = if up {
            current.saturating_sub(WHEEL_LINES)
        } else {
            current.saturating_add(WHEEL_LINES)
        };
        if tree {
            let to = to.min(self.term.drawn.diff_tree_scroll_max);
            self.change(|app| &mut app.view.diff.tree_scroll, to);
        } else {
            self.scroll_diff_to(to);
        }
    }

    pub(crate) fn diff_click(&mut self, hit: DiffHit) {
        match hit {
            DiffHit::Back => self.close_diff(),
            DiffHit::Body => self.change(|app| &mut app.view.diff.tree_focus, false),
            DiffHit::Tree => self.change(|app| &mut app.view.diff.tree_focus, true),
            DiffHit::File(index) => {
                self.change(|app| &mut app.view.diff.tree_focus, true);
                self.select_diff_file(index);
            }
        }
    }
}

pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    render::draw(frame, body, app, map);
}

#[cfg(test)]
mod tests;
