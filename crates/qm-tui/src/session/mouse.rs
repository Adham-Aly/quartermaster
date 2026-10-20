//! The session screen's mouse: `← Back`, `Interrupt`, `Expand`/`Collapse`, a tool-call run's
//! `▸ 3 tool calls` line, and the wheel over the session. Clicks on the session box or the input focus them, drags select and copy, as everywhere
//! (`input::mouse`).

use super::SessionHit;
use crate::app::App;
use crate::hit::Pane;
use crate::input::mouse::WHEEL_LINES;

impl App {
    /// A click on something of the session screen.
    pub(crate) fn session_click(&mut self, hit: SessionHit) {
        match hit {
            SessionHit::Back => self.leave_session(),
            SessionHit::Interrupt => self.interrupt_session(),
            SessionHit::Expand(id) => {
                self.focus_pane(Pane::Session);
                self.toggle_expanded(id);
            }
            SessionHit::ToolGroup(first) => {
                self.focus_pane(Pane::Session);
                self.toggle_tool_group(first);
            }
        }
    }

    /// The wheel over the session: up shows older lines.
    pub(crate) fn session_wheel(&mut self, up: bool) {
        let max = self.drawn.scroll_max_of(Pane::Session);
        let scroll = self.session_scroll();
        let to = if up {
            (scroll + WHEEL_LINES).min(max)
        } else {
            scroll.saturating_sub(WHEEL_LINES)
        };
        self.set_session_scroll(to);
    }
}
