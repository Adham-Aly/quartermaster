//! A worker's session.
//!
//! The screen owns the whole frame: the header with `← Back`, the
//! banner "Talking to this crewmate directly…", the session's lines, your input and the status
//! line with `Interrupt`. Your message goes to the worker as direct intervention
//! (`Command::Send { to: agent }`); the core neutralises it and slips it into a running turn.
//!
//! - [`draw`]: the screen, cell for cell.
//! - [`keys`]: its keys (Tab, ⏎, Esc, `i`, `e`, scrolling, `?`).
//! - [`mouse`]: Back, Interrupt, Expand/Collapse, a tool-call run's line, the wheel.
//!
//! Entering swaps the Bridge's draft (text, cursor and selection) out of `App::draft` and this
//! session's draft in; leaving swaps them back and tells the core
//! (`Command::ViewSession { agent: None }`).

pub(crate) mod draw;
pub(crate) mod keys;
pub(crate) mod mouse;

pub(crate) use draw::draw;

use crate::app::App;
use crate::bridge::pane_block;
use crate::hit::{Hit, Pane};
use crate::input::draft::Draft;
use crate::router::Screen;
use qm_types::api::{AgentRef, Command, SessionView};
use ratatui::style::Style;
use std::collections::{BTreeSet, HashMap};

/// The session screen has no status bar, so its status line starts with the
/// Bridge's `⚑ 1 approval` buttons while something waits for you, and only then.
pub(crate) const SESSION_SHOWS_ATTENTION: bool = true;

/// The banner, shown for as long as you're in a worker's session.
pub(crate) const BANNER: &str =
    "Talking to this crewmate directly. You override its supervisor; it will reconcile.";

/// Something on the session screen a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionHit {
    /// `← Back`.
    Back,
    /// The status line's `Interrupt`.
    Interrupt,
    /// The `Expand`/`Collapse` button of the line with this ID.
    Expand(u64),
    /// The `▸ 3 tool calls` line of the run of tool calls starting at the line with this ID.
    ToolGroup(u64),
}

/// A run of tool calls in a session, named by its agent and its first call's line ID, never by
/// where it is drawn: new calls joining the run keep its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ToolGroupKey {
    pub(crate) agent: AgentRef,
    pub(crate) first: u64,
}

/// The session screen's own state, in `View::session`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionUi {
    /// The session showing, while the screen is open.
    pub(crate) agent: Option<AgentRef>,
    /// How far the session is scrolled up from its newest line.
    pub(crate) scroll_from_bottom: usize,
    /// Lines whose detail is expanded, by ID.
    pub(crate) expanded: BTreeSet<u64>,
    /// The Bridge's draft while a session is open.
    pub(crate) bridge_draft: Draft,
    /// Where focus was on the Bridge, restored on the way back.
    pub(crate) bridge_focus: Option<Pane>,
    /// Each session's unsent draft.
    pub(crate) drafts: HashMap<AgentRef, Draft>,
}

impl App {
    /// The session view the core sent for the session showing, if it has arrived.
    pub(crate) fn session_view(&self) -> Option<&SessionView> {
        let Screen::Session(agent) = self.screen else {
            return None;
        };
        self.snapshot
            .session
            .as_ref()
            .filter(|view| view.agent == agent)
    }

    /// Whether the showing session takes your messages (not once its task is closed).
    pub(crate) fn session_open(&self) -> bool {
        match self.session_view() {
            Some(view) => view.open,
            // Until the core's view arrives, a crewmate's session takes text.
            None => matches!(self.screen, Screen::Session(AgentRef::Crewmate { .. })),
        }
    }

    /// Shows `agent`'s session: swaps the drafts, focuses the input and asks the core for it.
    pub(crate) fn enter_session(&mut self, agent: AgentRef) {
        if self.screen == Screen::Session(agent) {
            return;
        }
        self.leave_session();
        self.close_popups();
        self.view.menu = None;
        self.term.selection = None;
        self.term.pointer.selecting = false;
        self.view.task_overlay = false;
        let bridge_focus = self.focus;
        let ui = self.view.session.get_or_insert_with(SessionUi::default);
        ui.agent = Some(agent);
        ui.scroll_from_bottom = 0;
        ui.expanded.clear();
        ui.bridge_draft = std::mem::take(&mut self.draft);
        ui.bridge_focus = Some(bridge_focus);
        self.draft = ui.drafts.remove(&agent).unwrap_or_default();
        self.screen = Screen::Session(agent);
        self.focus = Pane::Input;
        self.command(Command::ViewSession { agent: Some(agent) });
    }

    /// Leaves the session showing, if one is: keeps its draft, puts the Bridge's back and tells
    /// the core. The caller picks the next screen.
    pub(crate) fn leave_session(&mut self) {
        let Screen::Session(agent) = self.screen else {
            return;
        };
        let draft = std::mem::take(&mut self.draft);
        let ui = self.view.session.get_or_insert_with(SessionUi::default);
        if !draft.is_empty() {
            ui.drafts.insert(agent, draft);
        }
        ui.agent = None;
        self.draft = std::mem::take(&mut ui.bridge_draft);
        self.focus = ui.bridge_focus.take().unwrap_or(Pane::Crew);
        self.term.selection = None;
        self.term.pointer.selecting = false;
        self.screen = Screen::Bridge;
        self.command(Command::ViewSession { agent: None });
    }

    /// The session's scroll offset, counted up from its newest line.
    pub(crate) fn session_scroll(&self) -> usize {
        self.view
            .session
            .as_ref()
            .map_or(0, |ui| ui.scroll_from_bottom)
    }

    pub(crate) fn set_session_scroll(&mut self, to: usize) {
        let ui = self.view.session.get_or_insert_with(SessionUi::default);
        if ui.scroll_from_bottom != to {
            ui.scroll_from_bottom = to;
            self.redraw();
        }
    }

    /// Expands or collapses the detail of line `id`.
    pub(crate) fn toggle_expanded(&mut self, id: u64) {
        let ui = self.view.session.get_or_insert_with(SessionUi::default);
        if !ui.expanded.remove(&id) {
            ui.expanded.insert(id);
        }
        self.redraw();
    }

    /// `i` or the `Interrupt` button: stops the worker's running turn, keeping its session.
    pub(crate) fn interrupt_session(&mut self) {
        let Screen::Session(agent) = self.screen else {
            return;
        };
        if self.session_view().is_some_and(|view| view.turn_running) {
            self.command(Command::Interrupt { agent });
        }
    }

    /// Shows or hides the calls of the tool-call run starting at line `first`, its
    /// `▸ 3 tool calls` line staying where it is on screen. A view pinned to the newest line
    /// stays pinned, scrolling only as far as keeps that line on screen.
    pub(crate) fn toggle_tool_group(&mut self, first: u64) {
        let Screen::Session(agent) = self.screen else {
            return;
        };
        let inner = self
            .term
            .drawn
            .hits
            .rect_of(Hit::Pane(Pane::Session))
            .map(|area| pane_block("Session", Style::default()).inner(area));
        let count = |app: &Self, width: u16| {
            let lines = app.session_view().map_or(&[][..], |v| v.lines.as_slice());
            draw::rows(lines, width, app)
        };
        let before = inner.map_or(0, |inner| count(self, inner.width).len());
        let key = ToolGroupKey { agent, first };
        let expanding = !self.view.tool_groups.remove(&key);
        if expanding {
            self.view.tool_groups.insert(key);
        }
        if let Some(inner) = inner {
            let rows = count(self, inner.width);
            let hit = Hit::Session(SessionHit::ToolGroup(first));
            if let Some(at) = rows.iter().position(|r| r.hits.iter().any(|h| h.2 == hit)) {
                let visible = usize::from(inner.height).max(1);
                let scroll = self.session_scroll();
                // Counted up from the newest row, so the rows shown or hidden under the line move
                // the offset with them; a view following the newest line keeps following it.
                let to = if expanding && scroll > 0 {
                    scroll + rows.len().saturating_sub(before)
                } else if expanding {
                    0
                } else {
                    scroll.saturating_sub(before.saturating_sub(rows.len()))
                };
                // Never so low that the line drops off the top.
                let below = rows.len() - 1 - at;
                let to = to
                    .max(below.saturating_sub(visible - 1))
                    .min(rows.len().saturating_sub(visible));
                let ui = self.view.session.get_or_insert_with(SessionUi::default);
                ui.scroll_from_bottom = to;
            }
        }
        self.redraw();
    }

    /// Whether the tool-call run starting at line `first` of the showing session is expanded.
    pub(crate) fn is_tool_group_expanded(&self, first: u64) -> bool {
        let Screen::Session(agent) = self.screen else {
            return false;
        };
        self.view
            .tool_groups
            .contains(&ToolGroupKey { agent, first })
    }

    /// Whether line `id` is expanded.
    pub(crate) fn is_expanded(&self, id: u64) -> bool {
        self.view
            .session
            .as_ref()
            .is_some_and(|ui| ui.expanded.contains(&id))
    }
}
