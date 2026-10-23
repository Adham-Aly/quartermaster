//! The Actions inbox's behaviour: its selection, its keys and the detail overlay (`Popup::Action`)
//! that Enter opens. The list itself is `widgets::actions`.
//!
//! Nothing in the list answers anything: a row only selects, and Enter or a click opens the
//! overlay. The overlay shows the item's card with the same buttons, and the same keys, as the
//! card in the conversation; no button is highlighted when it opens, so Enter does nothing until
//! you pick one with ←→. Answers go through the same commands the conversation's cards send.

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
/// Said when the open item is answered somewhere else.
pub(crate) const GONE: &str = "Answered elsewhere; nothing left to do here.";

/// The overlay's buttons for `item`: the card's own, unless answered, then "Go to".
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
    out.push(Button {
        label: GO_TO.to_owned(),
        hit: Hit::ActionButton(0),
        lit: false,
    });
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
    out.push(own);
    out
}

/// The overlay's line above the card: who asks, about what, and when.
fn meta(item: &ActionItem, now: jiff::Timestamp) -> String {
    let mut parts = Vec::new();
    if let Some(source) = crate::widgets::actions::source(item) {
        parts.push(source);
    }
    parts.push(crate::widgets::actions::age(item.at, now));
    parts.join(" · ")
}

/// The overlay's rows at `width` cells.
fn rows(app: &App, item: &ActionItem, width: u16) -> Vec<Row> {
    let pressed = app.view.action_pressed.as_ref() == Some(&item.key);
    let buttons = buttons(item, app.view.action_button, app.term.hover);
    let text = truncate(&meta(item, jiff::Timestamp::now()), usize::from(width));
    let mut out = vec![Row::plain(Line::from(Span::styled(text, theme::dim())))];
    out.extend(card::rows_with(
        &item.card,
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
        | CardAction::ApprovePlan { .. } => true,
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
        self.view.action_pressed = None;
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
        self.view.action_pressed = None;
        self.redraw();
    }

    /// After a snapshot: drops a selection whose item went (moving it to the nearest one), and
    /// closes the overlay when its item went, saying so unless you answered it here.
    pub(crate) fn actions_after_snapshot(&mut self, now: Instant) {
        if self.view.selected_action.is_some() && self.selected_action_index().is_none() {
            let last = self.snapshot.actions.len().checked_sub(1);
            let keep = last.map(|last| self.view.actions_scroll.min(last));
            self.view.selected_action = keep
                .and_then(|i| self.snapshot.actions.get(i))
                .map(|item| item.key.clone());
        }
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
            self.redraw();
        } else if matches!(action, CardAction::ShowDiff) {
            self.close_action();
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
        let anchor = item.anchor;
        self.close_action();
        match item.open {
            ActionOpen::Settings => self.choose_entry(crate::popups::MenuEntry::Settings),
            ActionOpen::Setup => self.show(crate::router::Screen::Setup),
            ActionOpen::Session { agent } => self.open_worker_session(agent),
            ActionOpen::Plan { task } => {
                self.command(qm_types::api::Command::OpenPlan { task });
            }
            ActionOpen::Conversation => {
                self.show_conversation();
                if let Some(message) = anchor
                    && self
                        .snapshot
                        .conversation
                        .iter()
                        .any(|line| line.id == message)
                {
                    self.change(|app| &mut app.view.card, Some(message));
                    self.change(|app| &mut app.scroll_from_bottom, 0);
                    self.reveal = Some(message);
                }
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

    /// Scrolls the Actions list so row `index` shows.
    pub(crate) fn keep_action_visible(&mut self, index: usize) {
        let shown = self
            .term
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == Pane::Actions)
            .map_or(1, |(area, _)| {
                usize::from(area.height.saturating_sub(2)).max(1)
            });
        let scroll = self.view.actions_scroll;
        let to = if index < scroll {
            index
        } else if index >= scroll + shown {
            index + 1 - shown
        } else {
            scroll
        };
        self.change(|app| &mut app.view.actions_scroll, to);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample;
    use crossterm::event::{Event, KeyEvent, KeyModifiers};
    use qm_types::api::{Command, Update};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_event(
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            Instant::now(),
        );
    }

    /// The sample with the Actions pane focused on the approval.
    fn inbox() -> App {
        let mut app = App::new();
        sample::install_actions(&mut app);
        let approval = app
            .snapshot
            .actions
            .iter()
            .position(|item| item.kind == qm_types::api::ActionKind::Approval)
            .unwrap();
        app.select_action(approval);
        screen(&mut app, 150, 45);
        app.take_commands();
        app
    }

    #[test]
    fn no_key_in_the_list_answers_anything() {
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('r'),
            KeyCode::Char('1'),
            KeyCode::Char('d'),
            KeyCode::Char('y'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char(' '),
        ] {
            let mut app = inbox();
            key(&mut app, code);
            assert_eq!(app.take_commands(), [] as [Command; 0], "{code:?}");
        }
    }

    #[test]
    fn focusing_the_pane_selects_its_first_row() {
        let mut app = inbox();
        app.view.selected_action = None;
        app.focus_pane(Pane::Crew);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.focus, Pane::Actions);
        assert_eq!(app.selected_action_index(), Some(0));
    }

    #[test]
    fn enter_opens_the_detail_with_nothing_highlighted() {
        let mut app = inbox();
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.popup(), Some(Popup::Action));
        assert_eq!(app.view.action_button, None);
        screen(&mut app, 150, 45);
        // A second Enter presses nothing: no button is picked yet.
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.take_commands(), [] as [Command; 0]);
        assert_eq!(app.popup(), Some(Popup::Action));
    }

    #[test]
    fn a_picked_button_or_its_letter_answers_the_right_action() {
        let mut app = inbox();
        key(&mut app, KeyCode::Enter);
        screen(&mut app, 150, 45);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Enter);
        let held = app.snapshot.actions[app.selected_action_index().unwrap()]
            .card
            .held
            .unwrap();
        assert_eq!(
            app.take_commands(),
            [Command::AnswerHeld {
                held,
                answer: HeldAnswer::Approve,
            }]
        );
        // Pressed once: the buttons dim and a second press sends nothing.
        key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }

    #[test]
    fn the_detail_closes_when_its_item_is_answered_elsewhere() {
        let mut app = inbox();
        app.view.sample = false;
        key(&mut app, KeyCode::Enter);
        let mut next = app.snapshot.clone();
        let open = app.view.action_open.clone();
        next.actions.retain(|item| Some(&item.key) != open.as_ref());
        app.on_update(Update::Snapshot(Box::new(next)));
        assert_eq!(app.popup(), None);
        assert_eq!(app.term.flash.as_ref().map(|f| f.text.as_str()), Some(GONE));
        // The selection moved to an item that is still there.
        assert!(app.selected_action_index().is_some());
    }

    #[test]
    fn guidance_reaches_its_held_action_while_models_are_unset() {
        let mut app = inbox();
        app.snapshot.model_setup_notice = Some("Choose model levels first.".to_owned());
        // A decision's "Retry with my guidance", as its card's button sends it.
        let held = qm_types::ids::HeldActionId::new(2);
        app.run_card_action(
            CardAction::Answer {
                answer: HeldAnswer::Choose {
                    choice: HeldChoice::RetryWithGuidance,
                    words: None,
                },
            },
            Some(held),
            None,
            AgentRef::Quartermaster,
            Instant::now(),
        );
        assert_eq!(app.focus, Pane::Input);
        assert!(!app.model_gated());
        app.set_input("use a local tmp dir".to_owned());
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.take_commands(),
            [Command::AnswerHeld {
                held,
                answer: HeldAnswer::Choose {
                    choice: HeldChoice::RetryWithGuidance,
                    words: Some("use a local tmp dir".to_owned()),
                },
            }]
        );
        // Plain messages to the Quartermaster stay gated.
        assert!(app.model_gated());
        app.set_input("hello".to_owned());
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }

    #[test]
    fn a_secondmate_question_replies_in_its_session() {
        let mut app = inbox();
        let i = app
            .snapshot
            .actions
            .iter()
            .position(|item| item.kind == qm_types::api::ActionKind::Question)
            .unwrap();
        app.select_action(i);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.popup(), None);
        assert!(matches!(
            app.shown_session(),
            Some(AgentRef::Secondmate { .. })
        ));
    }

    #[test]
    fn small_terminals_keep_the_inbox() {
        for (w, h) in [(80, 24), (60, 16)] {
            let mut app = inbox();
            let rows = screen(&mut app, w, h);
            assert!(rows.iter().any(|row| row.contains("Actions")), "{w}x{h}");
        }
    }
}
