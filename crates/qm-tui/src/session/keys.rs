//! A worker's session's keys. The Bridge handles the rest as on the conversation: `Tab` moves
//! focus through projects, crew, the session box, your input and the task pane; `⏎` in your
//! input sends to the worker (`⇧⏎`, `⌥⏎`, `Ctrl`+`J` a new line).
//!
//! | Key | Does |
//! |---|---|
//! | `Esc` | close the menu → clear a selection → back to the Quartermaster; never interrupts |
//! | `i` (session box) | interrupt the worker's running turn |
//! | `e` (session box) | expand or collapse the newest detail or tool-call run in view |
//! | ↑ ↓ `j` `k`, `PgUp` `PgDn`, `g` `G` (session box) | scroll |
//! | `⏎` (session box) | focus your input |
//!
//! `Ctrl`+`C`, `Ctrl`+`Z`, an open menu, `?` and `,` are handled before this, as everywhere.

use super::SessionHit;
use crate::app::App;
use crate::hit::{Hit, Pane};
use crossterm::event::KeyCode;
use qm_types::api::SessionLineKind;

impl App {
    /// A key on the focused session box.
    pub(crate) fn on_session_key(&mut self, code: KeyCode) {
        let max = self.term.drawn.scroll_max_of(Pane::Session);
        let page = self
            .term
            .drawn
            .session_rows
            .map_or(1, |area| usize::from(area.height.saturating_sub(1)).max(1));
        let scroll = self.session_scroll();
        // Counted up from the newest line, so "up" adds.
        let to = match code {
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
            return;
        }
        match code {
            KeyCode::Char('i') => self.interrupt_session(),
            KeyCode::Char('e') => self.expand_in_view(),
            KeyCode::Enter => self.focus_pane(Pane::Input),
            _ => {}
        }
    }

    /// `e`: toggles the newest line with a detail, or run of tool calls, whose button is in view.
    fn expand_in_view(&mut self) {
        let Some(view) = self.session_view() else {
            return;
        };
        let in_view = |hit| self.term.drawn.hits.rect_of(Hit::Session(hit)).is_some();
        let newest = view.lines.iter().enumerate().rev().find_map(|(i, line)| {
            let starts_run = line.kind == SessionLineKind::Tool
                && (i == 0 || view.lines[i - 1].kind != SessionLineKind::Tool);
            if starts_run && in_view(SessionHit::ToolGroup(line.id)) {
                Some(SessionHit::ToolGroup(line.id))
            } else if line.detail.is_some() && in_view(SessionHit::Expand(line.id)) {
                Some(SessionHit::Expand(line.id))
            } else {
                None
            }
        });
        match newest {
            Some(SessionHit::ToolGroup(first)) => self.toggle_tool_group(first),
            Some(SessionHit::Expand(id)) => self.toggle_expanded(id),
            _ => {}
        }
    }
}
