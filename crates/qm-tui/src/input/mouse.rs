//! The mouse: clicks, double-clicks, drags, the wheel, hover and right-clicks.
//!
//! Every action asks the last frame's map what is under the pointer, so what lights up on hover
//! is exactly what a click acts on.

use crate::app::{App, Quit};
use crate::hit::{Hit, Pane, StatusItem, TaskButton};
use crate::input::select;
use crate::view::{MenuTarget, Selection};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use qm_types::api::CardKind;
use ratatui::layout::Position;
use std::time::{Duration, Instant};

/// Lines one wheel notch scrolls.
pub(crate) const WHEEL_LINES: usize = 3;

/// The longest gap between two presses on the same thing that still makes a double-click.
pub(crate) const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// The left button's recent history.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pointer {
    /// The last left press, for telling a double-click.
    pub(crate) last_click: Option<(Hit, Instant)>,
    /// A drag-selection is in progress (the left button went down on selectable text).
    pub(crate) selecting: bool,
}

/// Whether a press on `hit` at `now` doubles the previous press `last`.
pub(crate) fn is_double(last: Option<(Hit, Instant)>, hit: Hit, now: Instant) -> bool {
    last.is_some_and(|(was, at)| was == hit && now.saturating_duration_since(at) <= DOUBLE_CLICK)
}

/// Whether a left press on `hit` may start a selection (text panes, or nothing clickable).
fn starts_selection(hit: Option<Hit>) -> bool {
    matches!(
        hit,
        None | Some(Hit::Pane(Pane::Conversation | Pane::Task | Pane::Session))
    )
}

/// Whether `hit` belongs to the folded task overlay.
fn in_overlay(hit: Option<Hit>) -> bool {
    matches!(
        hit,
        Some(Hit::Pane(Pane::Task) | Hit::TaskButton(_) | Hit::CloseOverlay)
    )
}

impl App {
    /// Applies a mouse event.
    pub(crate) fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) {
        let at = Position::new(mouse.column, mouse.row);
        let hit = self.drawn.hits.at(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Moved => self.hover_at(hit),
            MouseEventKind::Down(MouseButton::Left) => {
                self.hover_at(hit);
                self.left_press(hit, at, now);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.settings_dragging() => {
                self.settings_drag_to(hit);
            }
            MouseEventKind::Up(MouseButton::Left) if self.settings_dragging() => {
                self.settings_drop(hit);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(divider) = self.drag {
                    self.drag_to(divider, mouse.column);
                } else if self.pointer.selecting
                    && let Some(selection) = self.view.selection
                {
                    let moved = Selection {
                        head: at,
                        ..selection
                    };
                    self.change(|app| &mut app.view.selection, Some(moved));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.drag.take().is_some() {
                    self.save_prefs();
                    self.redraw();
                }
                if std::mem::take(&mut self.pointer.selecting) {
                    self.finish_selection(now);
                }
                self.hover_at(hit);
            }
            MouseEventKind::Down(MouseButton::Right) => self.right_press(hit, at),
            MouseEventKind::ScrollUp => self.wheel(at, true),
            MouseEventKind::ScrollDown => self.wheel(at, false),
            _ => {}
        }
    }

    /// Hover: what a click would act on. Over an open menu it also moves the menu's highlight.
    fn hover_at(&mut self, hit: Option<Hit>) {
        if let Some(Hit::MenuItem(i)) = hit {
            self.set_menu_cursor(i);
        }
        self.change(|app| &mut app.hover, hit);
    }

    fn left_press(&mut self, hit: Option<Hit>, at: Position, now: Instant) {
        if matches!(self.quit, Quit::Confirm { .. }) {
            if let Some(hit) = hit {
                self.click(hit);
            }
            return;
        }
        // An open menu takes the click: an item runs, the box swallows it, outside closes it.
        if self.view.menu.is_some() {
            match hit {
                Some(Hit::MenuItem(i)) => self.choose_menu_item(i, now),
                Some(Hit::Dialog) => {}
                _ => self.change(|app| &mut app.view.menu, None),
            }
            return;
        }
        // A click outside the folded task overlay closes it, then acts as usual (a popup over it
        // takes its clicks).
        if self.view.task_overlay && self.view.popup.is_none() && !in_overlay(hit) {
            self.close_overlay();
        }
        // A click without a drag clears the selection; one on text may start a new one.
        self.pointer.selecting = false;
        self.change(|app| &mut app.view.selection, None);
        if starts_selection(hit) {
            let rows = select::rows_for(&self.drawn, at);
            if select::on_text(&rows, at) {
                self.pointer.selecting = true;
                self.view.selection = Some(Selection {
                    anchor: at,
                    head: at,
                });
            }
        }
        let Some(hit) = hit else {
            self.pointer.last_click = None;
            return;
        };
        let double = is_double(self.pointer.last_click, hit, now);
        self.pointer.last_click = if double { None } else { Some((hit, now)) };
        self.left_click(hit, double, now);
    }

    /// What a left click on `hit` does.
    fn left_click(&mut self, hit: Hit, double: bool, now: Instant) {
        match hit {
            Hit::Project(i) => {
                self.change(|app| &mut app.view.selected_project, Some(i));
                self.focus_pane(Pane::Projects);
            }
            Hit::Crew(agent) => {
                self.focus_pane(Pane::Crew);
                // A single click selects (and opens the overlay when folded); a double-click
                // opens, like `o`.
                self.open_crew(agent, double, now);
            }
            Hit::CardButton { message, index } => self.press_card_button(message, index, now),
            Hit::TaskButton(TaskButton::OpenSession) => self.open_session(now),
            Hit::TaskButton(TaskButton::Stop) => self.stop_selected(),
            Hit::Status(StatusItem::Approvals) => self.jump_to_card(CardKind::Approval),
            Hit::Status(StatusItem::Decisions) => self.jump_to_card(CardKind::Decision),
            Hit::Status(StatusItem::Running) => self.focus_pane(Pane::Crew),
            Hit::CloseOverlay => self.close_overlay(),
            Hit::Settings(hit) => self.settings_click(hit, double, now),
            Hit::Setup(hit) => self.setup_click(hit, now),
            Hit::Session(hit) => self.session_click(hit),
            Hit::Diff(hit) => self.diff_click(hit),
            Hit::MenuItem(_) | Hit::Dialog => {}
            Hit::MenuButton
            | Hit::Popup(_)
            | Hit::Pane(_)
            | Hit::Divider(_)
            | Hit::ConfirmQuit
            | Hit::CancelQuit => {
                self.click(hit);
            }
        }
    }

    /// The release of a drag-selection: copies what it covers, or clears a mere click.
    fn finish_selection(&mut self, now: Instant) {
        let Some(selection) = self.view.selection else {
            return;
        };
        if selection.anchor == selection.head {
            self.change(|app| &mut app.view.selection, None);
            return;
        }
        let rows = select::rows_for(&self.drawn, selection.anchor);
        let text = select::text(&rows, selection);
        self.copy(&text, now);
    }

    /// A right-click on a crew or project row selects it and opens its menu at the pointer.
    fn right_press(&mut self, hit: Option<Hit>, at: Position) {
        if matches!(self.quit, Quit::Confirm { .. }) {
            return;
        }
        let target = match hit {
            Some(Hit::Crew(agent)) => MenuTarget::Crew(agent),
            Some(Hit::Project(i)) => MenuTarget::Project(i),
            Some(Hit::Settings(hit)) => return self.settings_right_click(hit, at),
            _ => return,
        };
        self.select_target(target);
        self.open_menu(target, at);
    }

    /// The wheel scrolls the pane under the pointer, never the focused one, and clears the
    /// selection.
    fn wheel(&mut self, at: Position, up: bool) {
        if self.view.menu.is_some() || matches!(self.quit, Quit::Confirm { .. }) {
            return;
        }
        if self.screen == crate::router::Screen::Diff && self.view.popup.is_none() {
            self.pointer.selecting = false;
            self.change(|app| &mut app.view.selection, None);
            self.diff_wheel(up);
            return;
        }
        let Some(pane) = self.drawn.scroll_pane_at(at.x, at.y) else {
            return;
        };
        self.pointer.selecting = false;
        self.change(|app| &mut app.view.selection, None);
        if pane == Pane::Conversation {
            // Counted up from the newest line.
            let max = self.drawn.conversation_scroll_max;
            let to = if up {
                (self.scroll_from_bottom + WHEEL_LINES).min(max)
            } else {
                self.scroll_from_bottom.saturating_sub(WHEEL_LINES)
            };
            self.change(|app| &mut app.scroll_from_bottom, to);
            return;
        }
        if pane == Pane::Session {
            self.session_wheel(up);
            return;
        }
        let max = self.drawn.scroll_max_of(pane);
        let slot: fn(&mut App) -> &mut usize = match pane {
            Pane::Projects => |app| &mut app.view.projects_scroll,
            Pane::Crew => |app| &mut app.view.crew_scroll,
            Pane::Task => |app| &mut app.view.task_scroll,
            Pane::Settings => |app| &mut app.view.settings.scroll,
            Pane::Help => |app| &mut app.view.help_scroll,
            Pane::Conversation | Pane::Input | Pane::Session => return,
        };
        let current = *slot(self);
        let to = if up {
            current.saturating_sub(WHEEL_LINES)
        } else {
            (current + WHEEL_LINES).min(max)
        };
        self.change(slot, to);
    }
}
