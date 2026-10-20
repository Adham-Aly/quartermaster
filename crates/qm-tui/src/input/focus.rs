//! Focus order and the `Esc` chain.

use crate::app::App;
use crate::hit::Pane;
use crate::router::Screen;
use qm_types::api::{AgentRef, Command};

/// The panes `Tab` visits, in order: the task pane only while it shows (not folded, or the
/// overlay is open).
pub(crate) fn order(folded: bool, overlay: bool) -> Vec<Pane> {
    let mut order = vec![Pane::Projects, Pane::Crew, Pane::Conversation, Pane::Input];
    if !folded || overlay {
        order.push(Pane::Task);
    }
    order
}

/// The pane after (or before) `from` in `order`; the first pane when `from` isn't in it.
pub(crate) fn next(order: &[Pane], from: Pane, forward: bool) -> Pane {
    let len = order.len();
    match order.iter().position(|&p| p == from) {
        Some(at) if forward => order[(at + 1) % len],
        Some(at) => order[(at + len - 1) % len],
        None => order[0],
    }
}

impl App {
    /// `Tab` / `⇧Tab` on the Bridge.
    pub(crate) fn cycle_focus(&mut self, forward: bool) {
        // A worker's session has two panes: its lines and your input.
        if let Screen::Session(_) = self.screen {
            let to = if self.focus == Pane::Input {
                Pane::Session
            } else {
                Pane::Input
            };
            let _ = forward;
            self.focus_pane(to);
            return;
        }
        if self.screen != Screen::Bridge {
            return;
        }
        let order = order(self.drawn.folded, self.view.task_overlay);
        self.focus_pane(next(&order, self.focus, forward));
    }

    /// Closes the task overlay, moving focus off the pane it took with it.
    pub(crate) fn close_overlay(&mut self) {
        self.change(|app| &mut app.view.task_overlay, false);
        if self.focus == Pane::Task && self.drawn.folded {
            self.focus_pane(Pane::Crew);
        }
    }

    /// `Esc`: the first that applies of close the menu, close the overlay, clear the selection,
    /// leave guidance or location mode, interrupt a running Quartermaster turn, back to the Bridge.
    pub(crate) fn escape(&mut self) {
        if self.view.menu.is_some() {
            self.change(|app| &mut app.view.menu, None);
        } else if self.view.task_overlay {
            self.close_overlay();
        } else if self.view.selection.is_some() {
            self.pointer.selecting = false;
            self.change(|app| &mut app.view.selection, None);
        } else if self.view.guidance_for.is_some() {
            self.change(|app| &mut app.view.guidance_for, None);
        } else if self.view.location_for.is_some() {
            self.change(|app| &mut app.view.location_for, None);
            self.input.clear();
            self.view.cursor = None;
        } else if self.screen == Screen::Help {
            self.close_help();
        } else if self.screen != Screen::Bridge {
            self.show(Screen::Bridge);
        } else if self.snapshot.turn_running {
            self.command(Command::Interrupt {
                agent: AgentRef::Quartermaster,
            });
        }
    }
}
