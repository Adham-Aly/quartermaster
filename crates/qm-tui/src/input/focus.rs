//! Focus order and the `Esc` chain.

use crate::app::App;
use crate::hit::Pane;
use crate::router::Screen;
use qm_types::api::{AgentRef, Command};

/// The panes `Tab` visits, in order: a worker's session box in the conversation's place while
/// one shows, and the task pane only while it shows (not folded, or the overlay is open).
pub(crate) fn order(folded: bool, overlay: bool, session: bool) -> Vec<Pane> {
    let middle = if session {
        Pane::Session
    } else {
        Pane::Conversation
    };
    let mut order = vec![Pane::Projects, Pane::Crew, middle, Pane::Input];
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
        if self.screen != Screen::Bridge {
            return;
        }
        let session = self.shown_session().is_some();
        let order = order(self.term.drawn.folded, self.view.task_overlay, session);
        self.focus_pane(next(&order, self.focus, forward));
    }

    /// Closes the task overlay, moving focus off the pane it took with it.
    pub(crate) fn close_overlay(&mut self) {
        self.change(|app| &mut app.view.task_overlay, false);
        if self.focus == Pane::Task && self.term.drawn.folded {
            self.focus_pane(Pane::Crew);
        }
    }

    /// `Esc`: the first that applies of close the menu, close the popup, close the overlay, clear
    /// the selection, clear the selection in your message while typing, leave guidance or
    /// location mode (for the Quartermaster's input), back to the Bridge, back from a worker's
    /// session to the Quartermaster's conversation, interrupt a running Quartermaster turn.
    pub(crate) fn escape(&mut self) {
        let session = self.shown_session();
        if self.view.menu.is_some() {
            self.change(|app| &mut app.view.menu, None);
        } else if !self.view.popups.is_empty() {
            self.close_popup();
        } else if self.view.task_overlay {
            self.close_overlay();
        } else if self.term.selection.is_some() {
            self.term.pointer.selecting = false;
            self.change(|app| &mut app.term.selection, None);
        } else if self.focus == Pane::Input && self.selection().is_some() {
            self.change(|app| &mut app.draft.anchor, None);
        } else if session.is_none() && self.view.guidance_for.is_some() {
            self.change(|app| &mut app.view.guidance_for, None);
        } else if session.is_none() && self.view.location_for.is_some() {
            self.change(|app| &mut app.view.location_for, None);
            self.set_input(String::new());
        } else if self.screen != Screen::Bridge {
            self.show(Screen::Bridge);
        } else if session.is_some() {
            // Never an interrupt here: `i` on the session box stops the worker.
            self.leave_session();
        } else if self.snapshot.turn_running {
            self.command(Command::Interrupt {
                agent: AgentRef::Quartermaster,
            });
        }
    }
}
