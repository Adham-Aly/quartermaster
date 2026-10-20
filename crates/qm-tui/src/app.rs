//! The UI's state and its event loop: keys, mouse, updates from the core, and quitting.

use crate::hit::{Divider, FrameMap, Hit, Pane};
use crate::input::{menu, prefs, select};
use crate::router::Screen;
use crate::terminal::TerminalGuard;
use crate::view::Flash;
use crate::{chrome, router};
use crossterm::event::{self, Event, KeyEventKind};
use qm_types::api::{Command, CoreClient, Snapshot, Update};
use qm_types::ids::MessageId;
use ratatui::Frame;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long the loop waits for input before checking the core's updates again. Updates are drained
/// without blocking, so this bounds how late one can show.
const POLL: Duration = Duration::from_millis(40);

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
    /// What the pointer is over, from the last frame's hit map.
    pub(crate) hover: Option<Hit>,
    /// The divider being dragged, if any.
    pub(crate) drag: Option<Divider>,
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
    pub(crate) quit: Quit,
    /// What the last frame drew.
    pub(crate) drawn: FrameMap,
    /// The terminal size at the last frame.
    pub(crate) size: (u16, u16),
    /// UI state: selection, menus, overlays.
    pub(crate) view: crate::view::View,
    /// Commands waiting to go to the core.
    outbox: Vec<Command>,
    /// Set when the UI should return.
    exit: bool,
    /// Set when something changed and the screen needs a new frame.
    dirty: bool,
    /// `Ctrl`+`Z` was pressed: the loop suspends to the shell before the next frame.
    pub(crate) suspend: bool,
    /// The left button's recent history: double-clicks and drag-selections.
    pub(crate) pointer: crate::input::mouse::Pointer,
    /// An OSC 52 copy to write to the terminal right after the next frame.
    pub(crate) osc52: Option<String>,
    /// The core rang the bell: how many BELs to write to the terminal (at most one).
    pub(crate) bells: u8,
    /// A card to scroll into view, a step per frame, until its buttons show.
    pub(crate) reveal: Option<MessageId>,
    /// Where column widths are remembered (`<data dir>/tui.toml`); `None` remembers nothing.
    pub(crate) prefs_path: Option<PathBuf>,
    /// The setup check screen: its terminal panes and what the keys act on.
    pub(crate) setup: crate::setup::SetupState,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            snapshot: Snapshot::default(),
            screen: Screen::Bridge,
            focus: Pane::Input,
            hover: None,
            drag: None,
            draft: crate::input::draft::Draft::default(),
            pasting: None,
            left_width: LEFT_DEFAULT,
            right_width: RIGHT_DEFAULT,
            scroll_from_bottom: 0,
            quit: Quit::Idle,
            drawn: FrameMap::default(),
            size: (0, 0),
            view: crate::view::View::default(),
            outbox: Vec::new(),
            exit: false,
            dirty: true,
            suspend: false,
            pointer: crate::input::mouse::Pointer::default(),
            osc52: None,
            bells: 0,
            reveal: None,
            prefs_path: None,
            setup: crate::setup::SetupState::default(),
        }
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
        self.view.flash = Some(Flash {
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

    /// Draws a frame and keeps its hit map for the input that follows.
    pub(crate) fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.size = (area.width, area.height);
        let mut map = FrameMap::default();
        // A worker's session owns the whole frame (no top bar or status bar).
        let body = if let Screen::Session(_) = self.screen {
            Some(area)
        } else {
            chrome::draw(frame, self, &mut map.hits)
        };
        if let Some(body) = body {
            router::draw(frame, body, self, &mut map);
        }
        if let Some(selection) = self.view.selection {
            let rows = select::rows_for(&map, selection.anchor);
            select::paint(frame.buffer_mut(), &rows, selection);
        }
        if let Some(open) = &self.view.menu {
            menu::draw(frame, open, &mut map.hits);
        }
        if let Quit::Confirm { working } = self.quit {
            chrome::draw_quit_dialog(frame, area, working, self.hover, &mut map.hits);
        }
        self.drawn = map;
        // Widening past the fold shows the task pane in place, so the overlay has nothing left
        // to do.
        if !self.drawn.folded {
            self.view.task_overlay = false;
        }
    }

    /// Work that needs the frame just drawn: the prompt field's scroll remembered, the Settings
    /// tab's scroll kept in range and its selection in view, and a card scrolled into view a step
    /// at a time.
    pub(crate) fn after_draw(&mut self) {
        if let Some(field) = &self.drawn.field {
            self.view.input_scroll = field.scroll;
        }
        if self.screen == Screen::Settings {
            let max = self.drawn.scroll_max_of(Pane::Settings);
            let mut scroll = self.view.settings.scroll.min(max);
            if std::mem::take(&mut self.view.settings.reveal)
                && let Some((row, shown)) = self.drawn.settings_selected
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
        self.after_draw_reveal();
    }

    fn after_draw_reveal(&mut self) {
        let Some(message) = self.reveal else { return };
        let shown = self
            .drawn
            .hits
            .rect_of(Hit::CardButton { message, index: 0 })
            .is_some();
        let max = self.drawn.conversation_scroll_max;
        if shown || self.scroll_from_bottom >= max {
            self.reveal = None;
            return;
        }
        let page = self
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
        match self.quit {
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
                    self.bells = 1;
                }
                self.snapshot = *snapshot;
                self.dirty = true;
                self.setup_after_snapshot();
            }
            Update::ConfirmQuit { working } => {
                self.quit = Quit::Confirm { working };
                self.dirty = true;
            }
            Update::Closing => self.exit = true,
            // Updates this build doesn't draw.
            _ => {}
        }
    }

    /// Time-driven changes: the quit window closing, a flash running out.
    pub(crate) fn tick(&mut self, now: Instant) {
        self.settings_tick(now);
        self.setup_tick(now);
        self.paste_tick(now);
        if let Quit::Armed { until } = self.quit
            && now >= until
        {
            self.quit = Quit::Idle;
            self.dirty = true;
        }
        if self.view.flash.as_ref().is_some_and(|f| now >= f.until) {
            self.view.flash = None;
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
        match self.quit {
            Quit::Idle => {
                self.quit = Quit::Armed {
                    until: now + QUIT_WINDOW,
                };
                self.dirty = true;
            }
            Quit::Armed { .. } => {
                self.outbox.push(Command::RequestQuit);
                self.quit = Quit::Requested;
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
        self.quit = Quit::Requested;
        self.dirty = true;
    }

    pub(crate) fn cancel_quit(&mut self) {
        self.quit = Quit::Idle;
        self.dirty = true;
    }

    pub(crate) fn show(&mut self, screen: Screen) {
        // Leaving a session puts the Bridge's draft back and sends `ViewSession { agent: None }`.
        if matches!(self.screen, Screen::Session(_)) && self.screen != screen {
            self.leave_session();
        }
        if self.screen != screen {
            self.view.help_from = None;
        }
        Self::set(&mut self.dirty, &mut self.screen, screen);
    }

    /// `?`: Help, remembering where it was opened from so `Esc` goes back there.
    pub(crate) fn open_help(&mut self) {
        let from = self.screen;
        self.show(Screen::Help);
        if from != Screen::Help {
            self.view.help_from = Some(from);
        }
    }

    /// `Esc` on Help: back to the screen `?` opened it from, a session reopened as it was.
    pub(crate) fn close_help(&mut self) {
        match self.view.help_from.take() {
            Some(Screen::Session(agent)) => self.enter_session(agent),
            Some(screen) => self.show(screen),
            None => self.show(Screen::Bridge),
        }
    }

    pub(crate) fn focus_pane(&mut self, pane: Pane) {
        Self::set(&mut self.dirty, &mut self.focus, pane);
    }

    pub(crate) fn click(&mut self, hit: Hit) {
        if matches!(self.quit, Quit::Confirm { .. }) {
            match hit {
                Hit::ConfirmQuit => self.confirm_quit(),
                Hit::CancelQuit => self.cancel_quit(),
                _ => {}
            }
            return;
        }
        match hit {
            Hit::Tab(screen) => self.show(screen),
            Hit::Pane(pane) => self.focus_pane(pane),
            Hit::Divider(divider) => {
                self.drag = Some(divider);
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

/// Runs the loop until you quit, the core closes, or the link drops.
pub(crate) fn run(client: &mut CoreClient) -> io::Result<()> {
    let prefs_path = prefs::default_path();
    let mut remembered = Some(prefs_path.as_deref().map(prefs::load).unwrap_or_default());
    if client.send(Command::Refresh).is_err() {
        return Ok(());
    }
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new();
    crate::sample::install_from_env(&mut app);
    loop {
        loop {
            match client.try_recv() {
                Ok(Some(update)) => app.on_update(update),
                Ok(None) => break,
                Err(_closed) => {
                    app.exit = true;
                    break;
                }
            }
        }
        // The settings screens queue their changes on their own; they go out with the rest.
        app.outbox.append(&mut app.view.settings.outbox);
        for command in app.outbox.drain(..) {
            if client.send(command).is_err() {
                app.exit = true;
            }
        }
        if app.exit {
            return Ok(());
        }
        if let Some(remembered) = remembered.take() {
            remembered.apply(&mut app);
            app.prefs_path.clone_from(&prefs_path);
        }
        if std::mem::take(&mut app.suspend) {
            terminal.suspend()?;
            app.dirty = true;
        }
        app.tick(Instant::now());
        if app.dirty {
            app.dirty = false;
            terminal.draw(|frame| app.draw(frame))?;
            app.after_draw();
            if let Some(osc52) = app.osc52.take() {
                // Outside the synchronized update, so the terminal acts on it at once.
                terminal.write_raw(osc52.as_bytes())?;
            }
        }
        if std::mem::take(&mut app.bells) > 0 {
            terminal.write_raw(b"\x07")?;
        }
        if event::poll(POLL)? {
            // Take everything already waiting before drawing again, so a burst of motion
            // events costs one frame, not one each.
            loop {
                app.on_event(event::read()?, Instant::now());
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}
