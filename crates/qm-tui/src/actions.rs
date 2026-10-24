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
        | CardAction::ApprovePlan { .. }
        | CardAction::ApproveReview { .. } => true,
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
                line.spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            });
        self.reset_press();
        self.redraw();
        self.flash(
            reason.map_or_else(|| NOT_TAKEN.to_owned(), |why| format!("Not done: {why}")),
            now,
        );
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
    use qm_types::api::{Card, CardLine, Styled};
    use qm_types::api::{Command, Update};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

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
    /// Opens the approval and presses Approve; returns its held action.
    fn press_approve(app: &mut App) -> qm_types::ids::HeldActionId {
        app.view.sample = false;
        key(app, KeyCode::Enter);
        screen(app, 150, 45);
        key(app, KeyCode::Char('a'));
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
        held
    }

    /// The next snapshot, with the open item's card changed by `edit`.
    fn with_open_card(app: &mut App, edit: impl Fn(&mut Card)) {
        let mut next = app.snapshot.clone();
        let open = app.view.action_open.clone();
        for item in &mut next.actions {
            if Some(&item.key) == open.as_ref() {
                edit(&mut item.card);
            }
        }
        app.on_update(Update::Snapshot(Box::new(next)));
    }

    #[test]
    fn a_refused_answer_says_why_and_can_be_tried_again() {
        let mut app = inbox();
        let held = press_approve(&mut app);
        // The core takes it (the card says it is working on it), then refuses: the card is back
        // to pending with the reason as an error line.
        with_open_card(&mut app, |card| {
            card.answered = Some("Working on it…".to_owned());
        });
        assert!(app.view.action_pressed.is_some());
        with_open_card(&mut app, |card| {
            card.answered = None;
            card.lines.push(CardLine::new(
                2,
                vec![Styled::new("the branch moved", qm_types::api::Tone::Error)],
            ));
        });
        assert_eq!(app.popup(), Some(Popup::Action));
        assert_eq!(app.view.action_pressed, None);
        assert_eq!(
            app.term.flash.as_ref().map(|f| f.text.as_str()),
            Some("Not done: the branch moved")
        );
        // The same overlay answers again, once.
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(
            app.take_commands(),
            [Command::AnswerHeld {
                held,
                answer: HeldAnswer::Approve,
            }]
        );
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }

    #[test]
    fn a_press_nothing_came_of_resets_after_a_wait() {
        let mut app = inbox();
        press_approve(&mut app);
        let now = Instant::now();
        app.after_press(now);
        assert!(app.view.action_pressed.is_some(), "still waiting");
        app.after_press(now + PRESS_WAIT + Duration::from_secs(1));
        assert_eq!(app.view.action_pressed, None);
        assert_eq!(
            app.term.flash.as_ref().map(|f| f.text.as_str()),
            Some(NOT_TAKEN)
        );
    }

    #[test]
    fn every_request_is_reachable_however_many() {
        let mut app = inbox();
        let template = app.snapshot.actions[0].clone();
        app.snapshot.actions = (0..250_i64)
            .map(|i| {
                let mut item = template.clone();
                item.key = ActionKey::Question {
                    message: qm_types::ids::MessageId::new(i),
                };
                item.title = format!("request number {i}");
                item
            })
            .collect();
        app.focus_pane(Pane::Actions);
        app.select_action(0);
        key(&mut app, KeyCode::End);
        assert_eq!(app.selected_action_index(), Some(249));
        // The first frame at a new size finds the row out of view; the next shows it.
        screen(&mut app, 120, 40);
        let rows = screen(&mut app, 120, 40);
        assert!(
            rows.iter().any(|row| row.contains("request number 249")),
            "{}",
            rows.join("\n")
        );
        key(&mut app, KeyCode::Home);
        for _ in 0..249 {
            key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.selected_action_index(), Some(249));
    }
}

/// The overlay as the only place cards show: every card type, Go to, Diff and back.
#[cfg(test)]
mod popup_tests {
    use super::*;
    use crate::router::Screen;
    use crate::sample;
    use crossterm::event::{
        Event, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use qm_types::api::{
        ActionKind, Card, CardButton, CardKind, ChatLine, Command, NoticeAnswer, Update,
    };
    use qm_types::ids::MessageId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.after_draw();
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

    fn click(app: &mut App, hit: Hit) {
        let area = app.term.drawn.hits.rect_of(hit).expect("drawn");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let mouse = MouseEvent {
                kind,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            };
            app.on_event(Event::Mouse(mouse), Instant::now());
        }
    }

    /// The chat sample, whose Actions hold an approval, a notice, a question from the
    /// Quartermaster, a plan, a secondmate's question and a setup item.
    fn chat() -> App {
        let mut app = App::new();
        sample::install_chat(&mut app);
        app.view.sample = false;
        screen(&mut app, 150, 45);
        app.take_commands();
        app
    }

    fn index(app: &App, pick: impl Fn(&ActionItem) -> bool) -> usize {
        app.snapshot
            .actions
            .iter()
            .position(pick)
            .expect("the item")
    }

    /// Opens the first item `pick` matches, draws it, and returns its index.
    fn open(app: &mut App, pick: impl Fn(&ActionItem) -> bool) -> usize {
        let i = index(app, pick);
        app.open_action(i);
        screen(app, 150, 45);
        i
    }

    fn labels(app: &mut App) -> Vec<String> {
        let rows = screen(app, 150, 45);
        let item = open_item(app).cloned().expect("open");
        buttons(&item, None, None)
            .into_iter()
            .map(|b| b.label)
            .filter(|label| rows.iter().any(|row| row.contains(label.as_str())))
            .collect()
    }

    #[test]
    fn the_chat_draws_no_card_of_any_kind() {
        for kind in [
            CardKind::Approval,
            CardKind::Decision,
            CardKind::Notice,
            CardKind::Question,
            CardKind::Plan,
        ] {
            for answered in [None, Some("Approved".to_owned())] {
                let mut app = App::new();
                sample::install_chat(&mut app);
                let mut card = Card::new(kind, "Zzheading");
                card.answered.clone_from(&answered);
                card.buttons = vec![CardButton::new("Zzbutton", 'z', CardAction::LeaveIt)];
                let mut line = ChatLine::new(
                    MessageId::new(77),
                    qm_types::domain::MessageSource::App,
                    qm_types::domain::MessageKind::Message,
                    jiff::Timestamp::now(),
                    "a line with a card",
                );
                line.card = Some(card);
                app.snapshot.conversation.push(line);
                let rows = screen(&mut app, 150, 45);
                assert!(rows.iter().any(|r| r.contains("a line with a card")));
                assert!(
                    !rows
                        .iter()
                        .any(|r| r.contains("Zzheading") || r.contains("Zzbutton")),
                    "{kind:?} {answered:?}"
                );
            }
        }
        // Nor the sample's own requests: they wait in Actions only.
        let mut app = chat();
        let rows = screen(&mut app, 150, 45);
        for text in ["Ask again", "1 · The session cache", "Copy path", "Approve"] {
            assert!(!rows.iter().any(|r| r.contains(text)), "{text}");
        }
    }

    #[test]
    fn a_single_click_or_enter_or_o_opens_a_row() {
        let mut app = chat();
        let i = index(&app, |item| item.kind == ActionKind::Notice);
        click(&mut app, Hit::ActionRow(i));
        assert_eq!(app.popup(), Some(Popup::Action));
        assert_eq!(
            app.view.action_open,
            Some(app.snapshot.actions[i].key.clone())
        );
        assert_eq!(app.take_commands(), [] as [Command; 0]);
        for code in [KeyCode::Enter, KeyCode::Char('o')] {
            let mut app = chat();
            app.focus_pane(Pane::Actions);
            app.select_action(i);
            key(&mut app, code);
            assert_eq!(app.popup(), Some(Popup::Action), "{code:?}");
        }
    }

    #[test]
    fn go_to_shows_only_for_items_that_live_elsewhere() {
        let mut app = chat();
        for kind in [ActionKind::Approval, ActionKind::Notice] {
            open(&mut app, |item| item.kind == kind);
            assert!(!labels(&mut app).contains(&GO_TO.to_owned()), "{kind:?}");
        }
        open(&mut app, |item| {
            item.kind == ActionKind::Question && item.from == Some(AgentRef::Quartermaster)
        });
        assert!(!labels(&mut app).contains(&GO_TO.to_owned()));
        for kind in [ActionKind::Plan, ActionKind::Setup] {
            open(&mut app, |item| item.kind == kind);
            assert!(labels(&mut app).contains(&GO_TO.to_owned()), "{kind:?}");
        }
        // The secondmate's question: Go to opens its session.
        open(&mut app, |item| {
            item.kind == ActionKind::Question && item.from != Some(AgentRef::Quartermaster)
        });
        let own = open_item(&app).unwrap().card.buttons.len();
        app.press_action_button(own, Instant::now());
        assert_eq!(app.popup(), None);
        assert!(matches!(
            app.shown_session(),
            Some(AgentRef::Secondmate { .. })
        ));
        // A plan's Go to opens its page.
        let mut app = chat();
        open(&mut app, |item| item.kind == ActionKind::Plan);
        let own = open_item(&app).unwrap().card.buttons.len();
        app.press_action_button(own, Instant::now());
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::OpenPlan { .. }]
        ));
    }

    #[test]
    fn diff_then_esc_reopens_the_popup_on_the_same_item() {
        let mut app = chat();
        let i = open(&mut app, |item| item.kind == ActionKind::Approval);
        let opened = app.snapshot.actions[i].key.clone();
        key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(app.popup(), None);
        let rows = screen(&mut app, 150, 45);
        let cells = |y: usize, x: usize, width: usize| {
            rows[y].chars().skip(x).take(width).collect::<String>()
        };
        assert_eq!(cells(3, 0, 12), "╭ Files · 6 ");
        assert_eq!(cells(5, 38, 7), " Before");
        assert_eq!(cells(5, 94, 6), " After");
        assert_eq!(cells(8, 38, 6), " 13 − ");
        assert_eq!(cells(8, 94, 6), " 13 + ");
        assert_eq!(cells(9, 38, 6), "      ");
        assert_eq!(
            app.term
                .drawn
                .hits
                .rect_of(Hit::Diff(crate::diff::DiffHit::Body)),
            Some(Rect::new(38, 6, 111, 37))
        );
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Bridge);
        assert_eq!(app.popup(), Some(Popup::Action));
        assert_eq!(app.view.action_open, Some(opened));
        assert_eq!(app.view.action_button, None);
    }

    #[test]
    fn esc_after_diff_reopens_nothing_once_the_item_went() {
        let mut app = chat();
        open(&mut app, |item| item.kind == ActionKind::Approval);
        key(&mut app, KeyCode::Char('d'));
        let mut next = app.snapshot.clone();
        next.actions
            .retain(|item| item.kind != ActionKind::Approval);
        app.on_update(Update::Snapshot(Box::new(next)));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Bridge);
        assert_eq!(app.popup(), None);
    }

    #[test]
    fn answering_from_the_diff_closes_it_without_reopening() {
        let mut app = chat();
        let i = open(&mut app, |item| item.kind == ActionKind::Approval);
        let held = app.snapshot.actions[i].card.held.unwrap();
        key(&mut app, KeyCode::Char('d'));
        app.snapshot.diff = Some(sample::diff());
        app.take_commands();
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(app.screen, Screen::Bridge);
        assert_eq!(app.popup(), None);
        assert!(app.take_commands().contains(&Command::AnswerHeld {
            held,
            answer: HeldAnswer::Approve,
        }));
    }

    #[test]
    fn a_decision_answers_and_its_guidance_names_the_card() {
        let mut app = App::new();
        sample::install_decision(&mut app);
        app.view.sample = false;
        screen(&mut app, 150, 45);
        assert_eq!(app.popup(), Some(Popup::Action));
        let item = open_item(&app).cloned().unwrap();
        assert_eq!(item.kind, ActionKind::Decision);
        key(&mut app, KeyCode::Char('3'));
        assert_eq!(
            app.take_commands(),
            [Command::AnswerHeld {
                held: item.card.held.unwrap(),
                answer: HeldAnswer::Choose {
                    choice: HeldChoice::Discard,
                    words: None,
                },
            }]
        );
        // Retry with guidance: the input, named after the card from Actions.
        let mut app = App::new();
        sample::install_decision(&mut app);
        app.view.sample = false;
        screen(&mut app, 150, 45);
        key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.popup(), None);
        assert_eq!(app.focus, Pane::Input);
        let rows = screen(&mut app, 150, 45);
        assert!(
            rows.iter()
                .any(|r| r.contains("your guidance for C-0047 fix(ci): flaky upload test")),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_question_option_sends_it_and_a_notice_answers() {
        let mut app = chat();
        open(&mut app, |item| {
            item.kind == ActionKind::Question && item.from == Some(AgentRef::Quartermaster)
        });
        key(&mut app, KeyCode::Char('2'));
        assert!(matches!(
            app.take_commands().as_slice(),
            [Command::Send { to: AgentRef::Quartermaster, text, .. }] if text == "The page cache"
        ));
        for (letter, answer) in [('a', NoticeAnswer::AskAgain), ('l', NoticeAnswer::LeaveIt)] {
            let mut app = chat();
            let i = open(&mut app, |item| item.kind == ActionKind::Notice);
            let ActionKey::Notice { message } = app.snapshot.actions[i].key else {
                panic!("a notice");
            };
            key(&mut app, KeyCode::Char(letter));
            assert_eq!(
                app.take_commands(),
                [Command::AnswerNotice { message, answer }]
            );
        }
    }

    #[test]
    fn a_question_without_options_opens_the_chat() {
        let mut app = chat();
        let i = index(&app, |item| item.kind == ActionKind::Notice);
        let mut card = Card::new(CardKind::Question, "Question");
        card.buttons = vec![CardButton::new(
            "Reply…",
            'r',
            CardAction::OpenChat {
                to: AgentRef::Quartermaster,
            },
        )];
        app.snapshot.actions[i].card = card;
        app.snapshot.actions[i].kind = ActionKind::Question;
        app.open_action(i);
        screen(&mut app, 150, 45);
        key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.popup(), None);
        assert_eq!(app.screen, Screen::Bridge);
        assert_eq!(app.shown_session(), None);
    }

    #[test]
    fn a_plan_opens_and_approves() {
        let mut app = chat();
        let i = open(&mut app, |item| item.kind == ActionKind::Plan);
        let ActionKey::Plan { task } = app.snapshot.actions[i].key else {
            panic!("a plan");
        };
        assert!(labels(&mut app).contains(&"Copy path".to_owned()));
        key(&mut app, KeyCode::Char('o'));
        assert_eq!(app.take_commands(), [Command::OpenPlan { task }]);
        let mut app = chat();
        let i = index(&app, |item| item.kind == ActionKind::Plan);
        app.snapshot.actions[i].card.buttons.push(CardButton::new(
            "Approve plan",
            'p',
            CardAction::ApprovePlan { task },
        ));
        app.open_action(i);
        screen(&mut app, 150, 45);
        key(&mut app, KeyCode::Char('p'));
        assert_eq!(app.take_commands(), [Command::ApprovePlan { task }]);
    }

    #[test]
    fn a_review_opens_its_report_and_goes_ahead() {
        let mut app = chat();
        let i = open(&mut app, |item| item.kind == ActionKind::Review);
        let ActionKey::Review { task } = app.snapshot.actions[i].key else {
            panic!("a review");
        };
        let shown = labels(&mut app);
        assert!(shown.contains(&"Open".to_owned()), "{shown:?}");
        assert!(shown.contains(&"Go ahead".to_owned()), "{shown:?}");
        key(&mut app, KeyCode::Char('o'));
        assert_eq!(app.take_commands(), [Command::OpenReport { task }]);
        let mut app = chat();
        open(&mut app, |item| item.kind == ActionKind::Review);
        key(&mut app, KeyCode::Char('g'));
        assert_eq!(app.take_commands(), [Command::ApproveReview { task }]);
    }

    #[test]
    fn a_setup_item_opens_setup() {
        let mut app = chat();
        open(&mut app, |item| item.kind == ActionKind::Setup);
        key(&mut app, KeyCode::Char('s'));
        assert_eq!(app.popup(), None);
        assert_eq!(app.screen, Screen::Setup);
    }

    #[test]
    fn card_keys_in_the_chat_send_nothing() {
        let mut app = chat();
        app.focus_pane(Pane::Conversation);
        for code in ['[', ']', 'a', 'd', 'r', 'l', '1', '2', '3'] {
            key(&mut app, KeyCode::Char(code));
            assert_eq!(app.take_commands(), [] as [Command; 0], "{code}");
        }
        assert_eq!(app.popup(), None);
    }
}
