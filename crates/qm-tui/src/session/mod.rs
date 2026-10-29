//! A worker's session, in the Bridge's middle panel.
//!
//! Opening one (a double-click or `⏎` on its crew row, the task pane's `Open session`, the crew
//! menu) swaps the Quartermaster's conversation and input for the session's: its box, titled
//! `C-0042 · {title}` with a `← Quartermaster` button, and your input to the worker. The projects,
//! crew and task columns stay as they are. Your message goes to the worker as direct intervention
//! (`Command::Send { to: agent }`); the core neutralises it and slips it into a running turn.
//! `Esc`, `← Quartermaster` or the Quartermaster's crew row brings the conversation back.
//!
//! Which session shows is `View::middle`, shared by every window.
//!
//! - [`draw`]: the middle panel, cell for cell.
//! - [`keys`]: the session box's keys (`i`, `e`, scrolling).
//! - [`mouse`]: `← Quartermaster`, Interrupt, Expand/Collapse, a tool-call run's line, the wheel.
//!
//! Opening swaps the Quartermaster's draft (text, cursor and selection) out of `App::draft` and
//! this session's draft in; going back swaps them back and tells the core
//! (`Command::ViewSession { agent: None }`).

pub(crate) mod draw;
pub(crate) mod keys;
pub(crate) mod mouse;

pub(crate) use draw::draw;

use crate::app::App;
use crate::hit::{Hit, Pane};
use crate::input::draft::Draft;
use crate::router::Screen;
use crate::view::Middle;
use qm_types::api::{AgentRef, Command, SessionView};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// The banner, shown for as long as you're in a crewmate's session.
pub(crate) const BANNER: &str =
    "Talking to this crewmate directly. You override its supervisor; it will reconcile.";

/// Something in a session a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionHit {
    /// `← Quartermaster` in the box's title: back to the Quartermaster's conversation.
    Back,
    /// `Interrupt` in the box's bottom border.
    Interrupt,
    /// The `Expand`/`Collapse` button of the line with this ID.
    Expand(u64),
    /// The `▸ 3 tool calls` line of the run of tool calls starting at the line with this ID.
    ToolGroup(u64),
}

/// A run of tool calls in a session, named by its agent and its first call's line ID, never by
/// where it is drawn: new calls joining the run keep its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ToolGroupKey {
    pub(crate) agent: AgentRef,
    pub(crate) first: u64,
}

/// The sessions' own state, in `View::session`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionUi {
    /// How far the session is scrolled up from its newest line.
    pub(crate) scroll_from_bottom: usize,
    /// Lines whose detail is expanded, by ID.
    pub(crate) expanded: BTreeSet<u64>,
    /// The Quartermaster's draft while a session shows.
    pub(crate) bridge_draft: Draft,
    /// Each session's unsent draft.
    pub(crate) drafts: HashMap<AgentRef, Draft>,
}

impl App {
    /// The worker whose session the middle panel shows, if one does.
    pub(crate) fn shown_session(&self) -> Option<AgentRef> {
        match self.view.middle {
            Middle::Session(agent) => Some(agent),
            Middle::Quartermaster => None,
        }
    }

    /// The session view the core sent for the session showing, if it has arrived.
    pub(crate) fn session_view(&self) -> Option<&SessionView> {
        let agent = self.shown_session()?;
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
            None => matches!(self.shown_session(), Some(AgentRef::Crewmate { .. })),
        }
    }

    /// Shows `agent`'s session in the middle panel, on the Bridge: swaps the drafts, focuses the
    /// input and asks the core for it.
    pub(crate) fn enter_session(&mut self, agent: AgentRef) {
        self.show(Screen::Bridge);
        self.close_popups();
        self.view.menu = None;
        self.view.task_overlay = false;
        if self.shown_session() == Some(agent) {
            self.focus_pane(Pane::Input);
            return;
        }
        self.leave_session();
        self.term.selection = None;
        self.term.pointer.selecting = false;
        let ui = self.view.session.get_or_insert_with(SessionUi::default);
        ui.scroll_from_bottom = 0;
        ui.expanded.clear();
        ui.bridge_draft = std::mem::take(&mut self.draft);
        self.draft = ui.drafts.remove(&agent).unwrap_or_default();
        self.view.middle = Middle::Session(agent);
        self.focus = Pane::Input;
        self.redraw();
        self.command(Command::ViewSession { agent: Some(agent) });
    }

    /// Puts the Quartermaster's conversation back in the middle panel, if a session shows there:
    /// keeps the session's draft, puts the Quartermaster's back and tells the core. Focus on the
    /// session box moves to the conversation; anywhere else it stays.
    pub(crate) fn leave_session(&mut self) {
        let Some(agent) = self.shown_session() else {
            return;
        };
        let draft = std::mem::take(&mut self.draft);
        let ui = self.view.session.get_or_insert_with(SessionUi::default);
        if !draft.is_empty() {
            ui.drafts.insert(agent, draft);
        }
        self.draft = std::mem::take(&mut ui.bridge_draft);
        if self.focus == Pane::Session {
            self.focus = Pane::Conversation;
        }
        self.term.selection = None;
        self.term.pointer.selecting = false;
        self.view.middle = Middle::Quartermaster;
        self.redraw();
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
        let Some(agent) = self.shown_session() else {
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
        let Some(agent) = self.shown_session() else {
            return;
        };
        let inner = self.term.drawn.session_rows;
        let count = |app: &Self, width: u16| {
            let lines = app.session_view().map_or(&[][..], |v| v.lines.as_slice());
            // As drawn: the compaction notes between the lines, the busy row unless compacting.
            let notes = crate::widgets::context_strip::chat_notes(app, agent);
            let busy = crate::widgets::context_strip::compacting_row(app, agent).is_none();
            draw::rows(lines, &notes, width, app, busy)
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
        let Some(agent) = self.shown_session() else {
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
