//! The UI's state: keys, mouse, updates from the core, and quitting. The host (`host`) drives it.

use crate::hit::{Divider, FrameMap, Hit, Pane};
use crate::input::{menu, select};
use crate::popups::Popup;
use crate::router::Screen;
use crate::term::Term;
use crate::view::Flash;
use crate::{chrome, router};
use crossterm::event::{Event, KeyEventKind};
use qm_types::api::{Command, Snapshot, Update};
use qm_types::ids::MessageId;
use qm_types::wire::ExitReason;
use ratatui::Frame;
use std::cell::Cell;
use std::time::{Duration, Instant};

/// How long the first `Ctrl`+`C` waits for the second.
const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// How long a flash ("Copied", "Not built yet.") stays in the status bar.
const FLASH_FOR: Duration = Duration::from_secs(2);

/// Default and limits of the Bridge's column widths, in cells. Terminal borders cost a cell
/// each, so the task column is 36 cells to give its text 32.
pub(crate) const LEFT_DEFAULT: u16 = 31;
pub(crate) const RIGHT_DEFAULT: u16 = 36;
pub(crate) const LEFT_RANGE: (u16, u16) = (20, 60);
pub(crate) const RIGHT_RANGE: (u16, u16) = (24, 70);

/// Where quitting stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quit {
    /// Not quitting.
    Idle,
    /// One `Ctrl`+`C` seen; a second before `until` quits.
    Armed { until: Instant },
    /// Asked the core; waiting for `Closing` (or `ConfirmQuit`).
    Requested,
    /// The core says agents are working; the dialog asks you to confirm.
    Confirm { working: u32 },
}

/// Everything the UI shows and remembers.
#[derive(Debug)]
pub(crate) struct App {
    /// The latest picture of the core; `Snapshot::default()` is the empty Bridge.
    pub(crate) snapshot: Snapshot,
    pub(crate) screen: Screen,
    pub(crate) focus: Pane,
    /// Your message being typed: its text, cursor and selection (no gesture state; that is in
    /// `pointer`).
    pub(crate) draft: crate::input::draft::Draft,
    /// A `Ctrl`+`V` waiting for the pasteboard.
    pub(crate) pasting: Option<crate::input::clipboard_image::Pasting>,
    /// Width of the left column (projects and crew), in cells.
    pub(crate) left_width: u16,
    /// Width of the task pane, in cells.
    pub(crate) right_width: u16,
    /// How far the conversation is scrolled up from its newest line.
    pub(crate) scroll_from_bottom: usize,
    /// This window's own state.
    pub(crate) term: Term,
    /// UI state: selection, menus, overlays.
    pub(crate) view: crate::view::View,
    /// Commands waiting to go to the core.
    outbox: Vec<Command>,
    /// Set when the UI should return.
    exit: bool,
    /// This UI is one window of a background server: quitting from a window detaches it.
    pub(crate) detachable: bool,
    /// Why the windows leave if the core closes after a stop asked from the menu: the server is
    /// stopping, or restarting.
    pub(crate) stop_reason: Option<ExitReason>,
    /// How many windows are attached to this UI; the status bar names the count when it's more
    /// than one.
    pub(crate) windows: usize,
    /// Set when something changed and the screen needs a new frame.
    dirty: bool,
    /// A card to scroll into view, a step per frame, until its buttons show.
    pub(crate) reveal: Option<MessageId>,
    /// The setup check screen: its terminal panes and what the keys act on.
    pub(crate) setup: crate::setup::SetupState,
    /// When the UI started: the busy indicator's steps count from here.
    started: Instant,
    /// The busy indicator's step (`widgets::spinner`).
    pub(crate) spin: usize,
    /// The frame being drawn, or the last one, shows a busy indicator, so ticks advance it.
    pub(crate) spinning: Cell<bool>,
    /// The frame shows an age or a running count of seconds, so ticks redraw it once a second.
    pub(crate) ages_shown: Cell<bool>,
    /// The second (since `started`) the last age redraw was for.
    ages_second: u64,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            snapshot: Snapshot::default(),
            screen: Screen::Bridge,
            focus: Pane::Input,
            draft: crate::input::draft::Draft::default(),
            pasting: None,
            left_width: LEFT_DEFAULT,
            right_width: RIGHT_DEFAULT,
            scroll_from_bottom: 0,
            term: Term::new(),
            view: crate::view::View::default(),
            outbox: Vec::new(),
            exit: false,
            detachable: false,
            stop_reason: None,
            windows: 0,
            dirty: true,
            reveal: None,
            setup: crate::setup::SetupState::default(),
            started: Instant::now(),
            spin: 0,
            spinning: Cell::new(false),
            ages_shown: Cell::new(false),
            ages_second: 0,
        }
    }

    /// Whether the UI should end: you quit, or the core closed.
    pub(crate) fn exiting(&self) -> bool {
        self.exit
    }

    /// Ends the UI (the core's link dropped).
    pub(crate) fn finish(&mut self) {
        self.exit = true;
    }

    /// Takes the commands waiting for the core, including the settings screens' own.
    pub(crate) fn take_commands(&mut self) -> Vec<Command> {
        let mut commands = std::mem::take(&mut self.outbox);
        commands.append(&mut self.view.settings.outbox);
        commands
    }

    /// Whether a new frame is wanted, clearing the request.
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Asks for a new frame.
    pub(crate) fn redraw(&mut self) {
        self.dirty = true;
    }

    /// Queues a command for the core.
    pub(crate) fn command(&mut self, command: Command) {
        self.outbox.push(command);
        self.dirty = true;
    }

    /// Shows `text` in the status bar for a couple of seconds.
    pub(crate) fn flash(&mut self, text: impl Into<String>, now: Instant) {
        self.term.flash = Some(Flash {
            text: text.into(),
            until: now + FLASH_FOR,
        });
        self.dirty = true;
    }

    /// Sets `slot` to `value`, asking for a frame when that changes it.
    pub(crate) fn change<T: PartialEq>(
        &mut self,
        slot: impl FnOnce(&mut Self) -> &mut T,
        value: T,
    ) {
        let slot = slot(self);
        if *slot != value {
            *slot = value;
            self.dirty = true;
        }
    }

    /// Draws a frame in this window's palette and keeps its hit map for the input that follows.
    pub(crate) fn draw(&mut self, frame: &mut Frame<'_>) {
        crate::theme::scoped(self.term.palette, || self.draw_frame(frame));
    }

    /// [`Self::draw`], with the palette in force.
    fn draw_frame(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.term.size = (area.width, area.height);
        self.spinning.set(false);
        self.ages_shown.set(false);
        let mut map = FrameMap::default();
        if let Some(body) = chrome::draw(frame, self, &mut map.hits) {
            router::draw(frame, body, self, &mut map);
            crate::popups::draw(frame, body, self, &mut map);
        }
        if let Some(selection) = self.term.selection {
            let rows = select::rows_for(&map, selection.anchor);
            select::paint(frame.buffer_mut(), &rows, selection);
        }
        // A menu whose row isn't drawn any more (scrolled away, or gone) closes.
        if let Some(open) = &self.view.menu
            && !menu::draw(frame, self, open, &mut map.hits)
        {
            self.view.menu = None;
        }
        if let Quit::Confirm { working } = self.term.quit {
            chrome::draw_quit_dialog(
                frame,
                area,
                working,
                self.quit_verb(),
                self.term.hover,
                &mut map.hits,
            );
        }
        let actions_before = self.actions_height();
        self.term.drawn = map;
        self.after_actions_resize(actions_before);
        // Widening past the fold shows the task pane in place, so the overlay has nothing left
        // to do.
        if !self.term.drawn.folded {
            self.view.task_overlay = false;
        }
        self.clamp_settings_scroll();
    }

    /// Keeps the Settings popup's scroll in range of the frame just drawn and its selection in
    /// view.
    fn clamp_settings_scroll(&mut self) {
        if self.popup() == Some(Popup::Settings) {
            let max = self.term.drawn.scroll_max_of(Pane::Settings);
            let mut scroll = self.view.settings.scroll.min(max);
            if std::mem::take(&mut self.view.settings.reveal)
                && let Some((row, shown)) = self.term.drawn.settings_selected
            {
                // With two rows of context either side where there are any (a tier's heading,
                // the box's border), so the first and last rows bring their surroundings.
                if row < scroll + 2 {
                    scroll = row.saturating_sub(2);
                } else if row + 3 > scroll + shown {
                    scroll = (row + 3).saturating_sub(shown);
                }
            }
            self.change(|app| &mut app.view.settings.scroll, scroll.min(max));
        }
    }

    /// Work that needs the frame just drawn: the prompt field's scroll remembered and a card
    /// scrolled into view a step at a time.
    pub(crate) fn after_draw(&mut self) {
        if let Some(field) = &self.term.drawn.field {
            self.view.input_scroll = field.scroll;
        }
        self.after_draw_reveal();
    }

    fn after_draw_reveal(&mut self) {
        let Some(message) = self.reveal else { return };
        let shown = self
            .term
            .drawn
            .hits
            .rect_of(Hit::CardButton { message, index: 0 })
            .is_some();
        let max = self.term.drawn.conversation_scroll_max;
        if shown || self.scroll_from_bottom >= max {
            self.reveal = None;
            return;
        }
        let page = self
            .term
            .drawn
            .hits
            .rect_of(Hit::Pane(Pane::Conversation))
            .map_or(1, |area| {
                usize::from(area.height.saturating_sub(2) / 2).max(1)
            });
        self.scroll_from_bottom = (self.scroll_from_bottom + page).min(max);
        self.dirty = true;
    }

    /// The short notice the status bar shows instead of its counts, if any.
    pub(crate) fn quit_notice(&self) -> Option<&'static str> {
        match self.term.quit {
            Quit::Armed { .. } if self.detachable => Some("Press Ctrl+C again to detach"),
            Quit::Armed { .. } => Some("Press Ctrl+C again to quit"),
            Quit::Requested => Some("Quitting…"),
            Quit::Idle | Quit::Confirm { .. } => None,
        }
    }

    fn set<T: PartialEq>(dirty: &mut bool, slot: &mut T, value: T) {
        if *slot != value {
            *slot = value;
            *dirty = true;
        }
    }

    /// Applies an update from the core.
    pub(crate) fn on_update(&mut self, update: Update) {
        match update {
            Update::Snapshot(_) if self.view.sample => {}
            Update::Snapshot(snapshot) => {
                self.view
                    .settings
                    .refresh(&snapshot.settings, Instant::now());
                if snapshot.bell > self.snapshot.bell {
                    self.term.bells = 1;
                }
                self.snapshot = *snapshot;
                self.dirty = true;
                self.setup_after_snapshot();
                self.actions_after_snapshot(Instant::now());
            }
            Update::ConfirmQuit { working } => {
                self.term.quit = Quit::Confirm { working };
                self.dirty = true;
            }
            Update::Closing => self.exit = true,
            // Updates this build doesn't draw.
            _ => {}
        }
    }

    /// Time-driven changes: the quit window closing, a flash running out, the busy indicator's
    /// next step (only while one is on screen).
    pub(crate) fn tick(&mut self, now: Instant) {
        if self.spinning.get() {
            let step =
                crate::widgets::spinner::step_at(now.saturating_duration_since(self.started));
            self.change(|app| &mut app.spin, step);
        }
        if self.ages_shown.get() {
            let second = now.saturating_duration_since(self.started).as_secs();
            self.change(|app| &mut app.ages_second, second);
        }
        self.settings_tick(now);
        self.after_press(now);
        self.setup_tick(now);
        self.paste_tick(now);
        if let Quit::Armed { until } = self.term.quit
            && now >= until
        {
            self.term.quit = Quit::Idle;
            self.dirty = true;
        }
        if self.term.flash.as_ref().is_some_and(|f| now >= f.until) {
            self.term.flash = None;
            self.dirty = true;
        }
    }

    /// Applies a terminal event.
    pub(crate) fn on_event(&mut self, event: Event, now: Instant) {
        self.setup_activity(now, matches!(event, Event::FocusGained));
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key, now),
            Event::Mouse(mouse) => self.on_mouse(mouse, now),
            Event::Paste(text) => self.on_paste(&text),
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    pub(crate) fn on_ctrl_c(&mut self, now: Instant) {
        match self.term.quit {
            Quit::Idle => {
                self.term.quit = Quit::Armed {
                    until: now + QUIT_WINDOW,
                };
                self.dirty = true;
            }
            // A window of a background server only leaves; stopping the server is its own command.
            Quit::Armed { .. } if self.detachable => {
                self.term.quit = Quit::Idle;
                self.term.detach = true;
                self.dirty = true;
            }
            Quit::Armed { .. } => {
                self.outbox.push(Command::RequestQuit);
                self.term.quit = Quit::Requested;
                self.dirty = true;
            }
            // The core hasn't answered: a third Ctrl+C leaves anyway, so a stuck core can never
            // trap you in the UI.
            Quit::Requested => self.exit = true,
            Quit::Confirm { .. } => self.confirm_quit(),
        }
    }

    pub(crate) fn confirm_quit(&mut self) {
        self.outbox.push(Command::Quit);
        self.term.quit = Quit::Requested;
        self.dirty = true;
    }

    /// Stops the server (or restarts it) from the menu: the core's quit path, with its question
    /// when agents are working; every window leaves for `reason` once it closes.
    pub(crate) fn request_server_stop(&mut self, reason: ExitReason) {
        self.outbox.push(Command::RequestQuit);
        self.term.quit = Quit::Requested;
        self.stop_reason = Some(reason);
        self.dirty = true;
    }

    /// The dialog's word for what it confirms: "quit" for the app, and for a background server
    /// what was asked of it.
    fn quit_verb(&self) -> &'static str {
        match (self.detachable, self.stop_reason) {
            (false, _) => "quit",
            (true, Some(ExitReason::Restarting)) => "restart",
            (true, _) => "stop",
        }
    }

    pub(crate) fn cancel_quit(&mut self) {
        self.term.quit = Quit::Idle;
        self.stop_reason = None;
        self.dirty = true;
    }

    /// Shows `screen`; going to another screen closes an open popup.
    pub(crate) fn show(&mut self, screen: Screen) {
        if self.screen != screen {
            self.close_popups();
        }
        Self::set(&mut self.dirty, &mut self.screen, screen);
    }

    pub(crate) fn focus_pane(&mut self, pane: Pane) {
        Self::set(&mut self.dirty, &mut self.focus, pane);
        // The Actions pane always has a row to act on while it has any.
        if pane == Pane::Actions && self.selected_action_index().is_none() {
            self.select_action(0);
        }
    }

    pub(crate) fn click(&mut self, hit: Hit) {
        if matches!(self.term.quit, Quit::Confirm { .. }) {
            match hit {
                Hit::ConfirmQuit => self.confirm_quit(),
                Hit::CancelQuit => self.cancel_quit(),
                _ => {}
            }
            return;
        }
        match hit {
            Hit::MenuButton => self.toggle_main_menu(),
            Hit::Popup(hit) => self.popup_click(hit),
            Hit::Pane(pane) => self.focus_pane(pane),
            Hit::Divider(divider) => {
                self.term.drag = Some(divider);
                self.dirty = true;
            }
            _ => {}
        }
    }

    pub(crate) fn drag_to(&mut self, divider: Divider, column: u16) {
        match divider {
            Divider::Left => {
                let width = column.clamp(LEFT_RANGE.0, LEFT_RANGE.1);
                Self::set(&mut self.dirty, &mut self.left_width, width);
            }
            Divider::Right => {
                let width = self
                    .term
                    .size
                    .0
                    .saturating_sub(column)
                    .saturating_sub(1)
                    .clamp(RIGHT_RANGE.0, RIGHT_RANGE.1);
                Self::set(&mut self.dirty, &mut self.right_width, width);
            }
        }
    }
}
