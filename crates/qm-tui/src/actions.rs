//! The Actions inbox's behaviour: its selection, its keys and the detail overlay (`Popup::Action`)
//! that opens an item. The list itself is `widgets::actions`.
//!
//! The overlay is the only place a card is shown: the chat draws none. Nothing in the list answers
//! anything: a click on a row, Enter or `o` opens the overlay, which shows the item's card with
//! its buttons and their keys; no button is highlighted when it opens, so Enter does nothing until
//! you pick one with ←→. "Go to" follows the card's buttons only for items that live elsewhere (a
//! plan's page, a secondmate's session, Settings or Setup). Diff remembers the item, so Esc on the
//! Diff screen brings its overlay back.

use crate::app::App;
use crate::hit::{FrameMap, Hit, Pane};
use crate::popups::Popup;
use crate::text::{cells, truncate};
use crate::theme;
use crate::widgets::card::{self, Button};
use crate::widgets::conversation::{Row, draw_rows};
use crossterm::event::KeyCode;
use qm_types::api::{
    ActionItem, ActionKey, ActionOpen, AgentRef, CardAction, HeldAnswer, HeldChoice,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use std::time::Instant;

/// The widest the overlay gets, in cells.
pub(crate) const WIDTH: u16 = 76;
/// The overlay's extra button, after the card's own.
pub(crate) const GO_TO: &str = "Go to";
/// How long a press waits for its item to go before the buttons come back, when the app shows
/// no progress on it (an answer that isn't a held action's, or a press the app never took).
pub(crate) const PRESS_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Said when a press ended with its item still waiting and no reason on the card.
pub(crate) const NOT_TAKEN: &str = "Nothing changed yet; you can try again.";

/// Said when the open item is answered somewhere else.
pub(crate) const GONE: &str = "Answered elsewhere; nothing left to do here.";

/// Whether the overlay offers "Go to": only for items that live somewhere else than the
/// Quartermaster's chat.
fn has_go_to(item: &ActionItem) -> bool {
    item.open != ActionOpen::Conversation
}

/// The overlay's buttons for `item`: the card's own, then "Go to" when it has one.
fn buttons(item: &ActionItem, lit: Option<usize>, hover: Option<Hit>) -> Vec<Button> {
    let mut out: Vec<Button> = Vec::new();
    if item.card.answered.is_none() {
        out.extend(
            item.card
                .buttons
                .iter()
                .map(|b| b.label.clone())
                .map(|label| Button {
                    label,
                    hit: Hit::ActionButton(0),
                    lit: false,
                }),
        );
    } else {
        // `rows_with` shows only the buttons past the card's own on an answered card.
        out.extend(item.card.buttons.iter().map(|b| Button {
            label: b.label.clone(),
            hit: Hit::ActionButton(0),
            lit: false,
        }));
    }
    if has_go_to(item) {
        out.push(Button {
            label: GO_TO.to_owned(),
            hit: Hit::ActionButton(0),
            lit: false,
        });
    }
    for (i, b) in out.iter_mut().enumerate() {
        b.hit = Hit::ActionButton(i);
        b.lit = lit == Some(i) || hover == Some(b.hit);
    }
    out
}

/// The indices of the overlay's buttons that can be pressed.
fn pressable(item: &ActionItem) -> Vec<usize> {
    let own = item.card.buttons.len();
    let mut out: Vec<usize> = if item.card.answered.is_none() {
        (0..own).collect()
    } else {
        Vec::new()
    };
    if has_go_to(item) {
        out.push(own);
    }
    out
}

/// The overlay's line above the card: who asks, about what, and when.
fn meta(app: &App, item: &ActionItem, now: jiff::Timestamp) -> String {
    let mut parts = Vec::new();
    if let Some(source) = crate::widgets::actions::source(app, item) {
        parts.push(source);
    }
    parts.push(crate::widgets::actions::age(item.at, now));
    parts.join(" · ")
}

/// The overlay's rows at `width` cells.
fn rows(app: &App, item: &ActionItem, width: u16) -> Vec<Row> {
    let pressed = app.view.action_pressed.as_ref() == Some(&item.key);
    let buttons = buttons(item, app.view.action_button, app.term.hover);
    let text = truncate(&meta(app, item, jiff::Timestamp::now()), usize::from(width));
    let mut out = vec![Row::plain(Line::from(Span::styled(text, theme::dim())))];
    let shown = crate::presentation::Presenter(&app.snapshot).card(&item.card);
    out.extend(card::rows_with(
        &shown,
        width,
        &buttons,
        app.term.hover,
        pressed,
    ));
    out
}

/// The open item, if it is still in the inbox.
fn open_item(app: &App) -> Option<&ActionItem> {
    let key = app.view.action_open.as_ref()?;
    app.snapshot.actions.iter().find(|item| &item.key == key)
}

/// The overlay's title.
pub(crate) fn title(app: &App) -> String {
    open_item(app).map_or_else(|| "Action".to_owned(), |item| item.title.clone())
}

/// The most the overlay wants within `body`, border and padding included.
pub(crate) fn size(app: &App, body: Rect) -> (u16, u16) {
    let width = WIDTH.min(body.width.saturating_sub(4)).max(20);
    let lines = open_item(app).map_or(1, |item| rows(app, item, width.saturating_sub(4)).len());
    (width, cells(lines) + 2)
}

/// Draws the open item into the overlay's `content`.
pub(crate) fn draw(frame: &mut Frame<'_>, content: Rect, app: &App, map: &mut FrameMap) {
    let Some(item) = open_item(app) else {
        return;
    };
    let rows = rows(app, item, content.width);
    let max = rows.len().saturating_sub(usize::from(content.height));
    map.scroll_areas.push((content, Pane::ActionOverlay));
    map.scroll_max.push((Pane::ActionOverlay, max));
    draw_rows(frame, content, rows, app.view.action_scroll.min(max), map);
}

/// Whether pressing `action` sends an answer (the overlay then waits for the item to go).
const fn answers(action: &CardAction) -> bool {
    match action {
        CardAction::Answer {
            answer:
                HeldAnswer::Choose {
                    choice: HeldChoice::RetryWithGuidance,
                    ..
                },
        } => false,
        CardAction::Answer { .. }
        | CardAction::AskAgain
        | CardAction::LeaveIt
        | CardAction::Reply { .. }
        | CardAction::ApprovePlan { .. }
        | CardAction::ApproveReview { .. }
        | CardAction::DismissReport { .. } => true,
        _ => false,
    }
}

impl App {
    /// Whether the model-setup notice takes the Quartermaster's input: until you choose model
    /// levels, nothing is sent to the Quartermaster. Guidance for a held action and a project's
    /// new location go to their held action's handler, not to the model, so they stay open.
    pub(crate) fn model_gated(&self) -> bool {
        self.snapshot.model_setup_notice.is_some()
            && self.view.guidance_for.is_none()
            && self.view.location_for.is_none()
    }

    /// The selected item's index, if it is still in the inbox.
    pub(crate) fn selected_action_index(&self) -> Option<usize> {
        let key = self.view.selected_action.as_ref()?;
        self.snapshot
            .actions
            .iter()
            .position(|item| &item.key == key)
    }

    /// Selects the item at `index`, by its key so the selection follows it.
    pub(crate) fn select_action(&mut self, index: usize) {
        let key = self
            .snapshot
            .actions
            .get(index)
            .map(|item| item.key.clone());
        if key.is_some() {
            self.change(|app| &mut app.view.selected_action, key);
        }
    }

    /// Opens the overlay on the item at `index`, with no button highlighted.
    pub(crate) fn open_action(&mut self, index: usize) {
        let Some(key) = self
            .snapshot
            .actions
            .get(index)
            .map(|item| item.key.clone())
        else {
            return;
        };
        self.change(|app| &mut app.view.selected_action, Some(key.clone()));
        self.view.action_open = Some(key);
        self.view.action_button = None;
        self.reset_press();
        self.view.action_scroll = 0;
        self.open_popup(Popup::Action);
    }

    /// Closes the overlay, if it is open.
    pub(crate) fn close_action(&mut self) {
        if self.popup() == Some(Popup::Action) {
            self.close_popup();
        }
        self.view.popups.retain(|open| open.popup != Popup::Action);
        self.view.action_open = None;
        self.view.action_button = None;
        self.reset_press();
        self.redraw();
    }

    /// Forgets a press: the overlay's buttons work again.
    fn reset_press(&mut self) {
        self.view.action_pressed = None;
        self.view.action_wait = None;
    }

    /// A press whose item is still here: while the app shows it being carried out ("Working on
    /// it…") the buttons stay dim. When it is back to waiting after that, the attempt failed or
    /// was refused, so the buttons come back with the card's reason; with no progress shown at
    /// all, they come back after [`PRESS_WAIT`]. The app still refuses a second answer to
    /// anything already answered or being carried out.
    pub(crate) fn after_press(&mut self, now: Instant) {
        let Some(key) = self.view.action_pressed.clone() else {
            return;
        };
        let Some(item) = self.snapshot.actions.iter().find(|item| item.key == key) else {
            return;
        };
        if item.card.answered.is_some() {
            if let Some(wait) = &mut self.view.action_wait {
                wait.saw_busy = true;
            }
            return;
        }
        let ended = self.view.action_wait.is_none_or(|wait| {
            wait.saw_busy || now.saturating_duration_since(wait.at) >= PRESS_WAIT
        });
        if !ended {
            return;
        }
        let reason = item
            .card
            .lines
            .iter()
            .rev()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|s| s.tone == qm_types::api::Tone::Error)
            })
            .map(|line| {
                (
                    line.spans
                        .iter()
                        .map(|s| s.text.as_str())
                        .collect::<String>(),
                    line.verbatim,
                )
            });
        self.reset_press();
        self.redraw();
        let (text, verbatim) = reason.map_or_else(
            || (NOT_TAKEN.to_owned(), false),
            |(why, verbatim)| (format!("Not done: {why}"), verbatim),
        );
        self.flash(text, now);
        if let Some(flash) = &mut self.term.flash {
            flash.verbatim = verbatim;
        }
    }

    /// After a snapshot: drops a selection whose item went (moving it to the nearest one), and
    /// closes the overlay when its item went, saying so unless you answered it here.
    pub(crate) fn actions_after_snapshot(&mut self, now: Instant) {
        if self.view.selected_action.is_some() && self.selected_action_index().is_none() {
            let last = self.snapshot.actions.len().checked_sub(1);
            // The item that was at the top of the section, or the last.
            let scroll = self.view.actions_scroll;
            let top = self
                .term
                .drawn
                .action_spots
                .iter()
                .position(|lines| lines.end > scroll);
            let keep = last.map(|last| top.unwrap_or(last).min(last));
            self.view.selected_action = keep
                .and_then(|i| self.snapshot.actions.get(i))
                .map(|item| item.key.clone());
        }
        self.after_press(now);
        if self.view.action_open.is_some() && open_item(self).is_none() {
            let pressed = self.view.action_pressed.is_some();
            let shown = self
                .view
                .popups
                .iter()
                .any(|open| open.popup == Popup::Action);
            self.close_action();
            if shown && !pressed {
                self.flash(GONE, now);
            }
        }
    }

    /// Presses the overlay's button `index`.
    pub(crate) fn press_action_button(&mut self, index: usize, now: Instant) {
        let Some(item) = open_item(self).cloned() else {
            return;
        };
        if self.view.action_pressed.as_ref() == Some(&item.key)
            || !pressable(&item).contains(&index)
        {
            return;
        }
        let Some(button) = item.card.buttons.get(index) else {
            self.go_to_action(&item);
            return;
        };
        let action = button.action.clone();
        if answers(&action) {
            self.view.action_pressed = Some(item.key.clone());
            self.view.action_wait = Some(crate::view::PressWait {
                at: now,
                saw_busy: false,
            });
            self.redraw();
        } else if matches!(action, CardAction::ShowDiff) {
            self.close_action();
            // Esc on the Diff screen brings this overlay back.
            self.view.diff_from = Some(item.key.clone());
        }
        let reply_to = item.from.unwrap_or(AgentRef::Quartermaster);
        let message = item.anchor.or(match item.key {
            ActionKey::Notice { message } | ActionKey::Question { message } => Some(message),
            _ => None,
        });
        self.run_card_action(action, item.card.held, message, reply_to, now);
    }

    /// "Go to": where the item lives.
    fn go_to_action(&mut self, item: &ActionItem) {
        self.close_action();
        match item.open {
            ActionOpen::Settings => self.choose_entry(crate::popups::MenuEntry::Settings),
            ActionOpen::Setup => self.show(crate::router::Screen::Setup),
            ActionOpen::Session { agent } => self.open_worker_session(agent),
            ActionOpen::Plan { task } => {
                self.command(qm_types::api::Command::OpenPlan { task });
            }
            // Anything this build doesn't know: the conversation.
            _ => self.show_conversation(),
        }
    }

    /// The Quartermaster's conversation in the middle, focused.
    fn show_conversation(&mut self) {
        self.show(crate::router::Screen::Bridge);
        self.leave_session();
        self.focus_pane(Pane::Conversation);
    }

    /// The Quartermaster's input, focused, for guidance or a new location.
    pub(crate) fn show_input(&mut self) {
        self.close_action();
        self.show(crate::router::Screen::Bridge);
        self.leave_session();
    }

    /// A key in the overlay.
    pub(crate) fn on_action_key(&mut self, code: KeyCode, now: Instant) -> bool {
        let Some(item) = open_item(self).cloned() else {
            return false;
        };
        let pressable = pressable(&item);
        let max = self.term.drawn.scroll_max_of(Pane::ActionOverlay);
        match code {
            KeyCode::Left | KeyCode::BackTab | KeyCode::Right | KeyCode::Tab
                if pressable.is_empty() => {}
            KeyCode::Left | KeyCode::BackTab => {
                let at = self
                    .view
                    .action_button
                    .and_then(|b| pressable.iter().position(|&i| i == b));
                let next = at.map_or(pressable.len() - 1, |at| at.saturating_sub(1));
                self.change(|app| &mut app.view.action_button, Some(pressable[next]));
            }
            KeyCode::Right | KeyCode::Tab => {
                let at = self
                    .view
                    .action_button
                    .and_then(|b| pressable.iter().position(|&i| i == b));
                let next = at.map_or(0, |at| (at + 1).min(pressable.len() - 1));
                self.change(|app| &mut app.view.action_button, Some(pressable[next]));
            }
            KeyCode::Enter => {
                if let Some(index) = self.view.action_button {
                    self.press_action_button(index, now);
                }
            }
            KeyCode::Up => {
                let to = self.view.action_scroll.min(max).saturating_sub(1);
                self.change(|app| &mut app.view.action_scroll, to);
            }
            KeyCode::Down => {
                let to = (self.view.action_scroll + 1).min(max);
                self.change(|app| &mut app.view.action_scroll, to);
            }
            KeyCode::PageUp | KeyCode::Home => self.change(|app| &mut app.view.action_scroll, 0),
            KeyCode::PageDown | KeyCode::End => self.change(|app| &mut app.view.action_scroll, max),
            KeyCode::Char(c) if item.card.answered.is_none() => {
                let Some(index) = item
                    .card
                    .buttons
                    .iter()
                    .position(|b| b.key.eq_ignore_ascii_case(&c))
                else {
                    return false;
                };
                self.press_action_button(index, now);
            }
            _ => return false,
        }
        true
    }

    /// A key on the focused Actions pane. Only moves and opens; never answers.
    pub(crate) fn on_actions_key(&mut self, code: KeyCode) -> bool {
        let len = self.snapshot.actions.len();
        if len == 0 {
            return false;
        }
        let current = self.selected_action_index();
        let page = self
            .term
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == Pane::Actions)
            .map_or(1, |(area, _)| {
                usize::from(area.height.saturating_sub(3)).max(1)
            });
        let to = match code {
            KeyCode::Up | KeyCode::Char('k') => current.map_or(len - 1, |i| i.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => current.map_or(0, |i| (i + 1).min(len - 1)),
            KeyCode::PageUp => current.map_or(0, |i| i.saturating_sub(page)),
            KeyCode::PageDown => current.map_or(0, |i| (i + page).min(len - 1)),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => len - 1,
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(i) = current {
                    self.open_action(i);
                }
                return true;
            }
            _ => return false,
        };
        self.select_action(to);
        self.keep_action_visible(to);
        true
    }

    /// The inbox's height in the frame last drawn, if it was drawn.
    pub(crate) fn actions_height(&self) -> Option<u16> {
        self.term
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == Pane::Actions)
            .map(|(area, _)| area.height)
    }

    /// After a draw: when the inbox changed height (a resize, or a pane above it grew), brings
    /// the selected row back into view. Only then, so the wheel can still scroll away from it.
    pub(crate) fn after_actions_resize(&mut self, before: Option<u16>) {
        if self.actions_height() != before
            && let Some(index) = self.selected_action_index()
        {
            self.keep_action_visible(index);
        }
    }

    /// Scrolls Needs you so item `index` shows, by its lines as last drawn.
    pub(crate) fn keep_action_visible(&mut self, index: usize) {
        let Some(lines) = self.term.drawn.action_spots.get(index).cloned() else {
            return;
        };
        let shown = self
            .term
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == Pane::Actions)
            .map_or(lines.len(), |(area, _)| {
                // Less the section's heading and the blank row under it.
                usize::from(area.height.saturating_sub(2)).max(1)
            });
        let scroll = self.view.actions_scroll;
        let to = if lines.start < scroll {
            lines.start
        } else if lines.end > scroll + shown {
            (lines.end - shown).min(lines.start)
        } else {
            scroll
        };
        self.change(|app| &mut app.view.actions_scroll, to);
    }

    /// Opens the card of the request `key` (a click on its notification); if it has been answered
    /// since, shows the inbox.
    pub(crate) fn open_action_key(&mut self, key: &ActionKey) {
        self.show(crate::router::Screen::Bridge);
        self.focus_pane(Pane::Actions);
        if let Some(i) = self
            .snapshot
            .actions
            .iter()
            .position(|item| &item.key == key)
        {
            self.keep_action_visible(i);
            self.open_action(i);
        }
    }

    /// Focuses the inbox on its newest item of `kind` (the status bar's counters).
    pub(crate) fn jump_to_action(&mut self, kind: qm_types::api::ActionKind) {
        self.show(crate::router::Screen::Bridge);
        self.focus_pane(Pane::Actions);
        let newest = self
            .snapshot
            .actions
            .iter()
            .enumerate()
            .filter(|(_, item)| item.kind == kind)
            .max_by_key(|(_, item)| item.at)
            .map(|(i, _)| i);
        if let Some(i) = newest {
            self.select_action(i);
            self.keep_action_visible(i);
        }
    }
}
