//! Popups over the current screen, and the top bar's menu that opens them.
//!
//! Settings and Help open as popup windows over whatever is showing (the Bridge, a session, the
//! setup check, a diff); the screen beneath keeps its focus, so closing a popup (`Esc`, its ` ✕ `,
//! or a click outside it) puts you back where you were. The window itself is
//! `widgets::popup`; the menu's list is `widgets::dropdown`, through the context-menu machinery
//! (`input::menu`, target `MenuTarget::Main`).
//!
//! - A new popup is a [`Popup`] variant, its title and size ([`Popup::title`], [`area`]), a line
//!   in [`draw`], and its keys in [`App::on_popup_key`].
//! - A new menu entry is a [`MenuEntry`] variant, a place in [`MenuEntry::ALL`], and what choosing
//!   it does in [`App::open_entry`].
//! - A badge (a red dot that says something there needs you) on an entry: make
//!   [`App::entry_badge`] say so, for instance by putting the entry in `View::badges`. The menu
//!   button shows a dot when any entry does, and an entry's popup shows one in its title.

use crate::app::App;
use crate::hit::FrameMap;
use crate::settings::Tab;
use crate::widgets::popup::{self as window, PopupHit, Window};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use std::time::Instant;

/// The widest the Settings popup gets, in cells.
const SETTINGS_WIDTH: u16 = 124;
/// The widest the Help popup gets, in cells.
const HELP_WIDTH: u16 = 104;

/// A window that opens over the current screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Popup {
    /// Settings and accounts, with its tabs.
    Settings,
    /// Keys and mouse help.
    Help,
}

impl Popup {
    /// The title in the window's top border.
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Help => "Help",
        }
    }

    /// The menu entry whose badge the popup's title shows.
    const fn entry(self) -> MenuEntry {
        match self {
            Self::Settings => MenuEntry::Settings,
            Self::Help => MenuEntry::Help,
        }
    }
}

/// An entry in the top bar's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuEntry {
    Settings,
    ModelLevels,
    Routing,
    Help,
}

impl MenuEntry {
    /// The menu's entries, top to bottom.
    pub(crate) const ALL: [Self; 4] =
        [Self::Settings, Self::ModelLevels, Self::Routing, Self::Help];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::ModelLevels => "Model levels",
            Self::Routing => "Routing",
            Self::Help => "Help: keys and mouse",
        }
    }

    /// The key that opens it from anywhere you aren't typing, shown beside it; empty for none.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Settings => ",",
            Self::Help => "?",
            Self::ModelLevels | Self::Routing => "",
        }
    }

    /// The entry labelled `label`.
    pub(crate) fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|entry| entry.label() == label)
    }
}

/// Where `popup` sits within `body` (the area between the top bar and the status bar).
fn area(popup: Popup, body: Rect) -> Rect {
    match popup {
        Popup::Settings => window::centered(body, SETTINGS_WIDTH, body.height),
        Popup::Help => {
            // As tall as its lines at the width it gets (border and padding included), at most
            // the body.
            let width = window::centered(body, HELP_WIDTH, body.height).width;
            let lines = crate::placeholder::lines(usize::from(width.saturating_sub(4))).len();
            let height = u16::try_from(lines + 2).unwrap_or(u16::MAX);
            window::centered(body, HELP_WIDTH, height)
        }
    }
}

/// Draws the open popup, if any, over `body` and records what it holds.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    let Some(popup) = app.view.popup else {
        return;
    };
    let area = area(popup, body);
    let window = Window::new(popup.title()).badge(app.entry_badge(popup.entry()));
    let inner = window::draw(frame, area, window, app.hover, map);
    // One cell of padding left and right, as in every box.
    let content = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    match popup {
        Popup::Settings => crate::settings::draw(frame, content, app, map),
        Popup::Help => crate::placeholder::draw_help(frame, content, app, map),
    }
}

impl App {
    /// Whether `entry` shows a badge: something there needs you.
    pub(crate) fn entry_badge(&self, entry: MenuEntry) -> bool {
        self.view.badges.contains(&entry)
    }

    /// Whether the menu button shows a badge: some entry does.
    pub(crate) fn menu_badge(&self) -> bool {
        MenuEntry::ALL
            .into_iter()
            .any(|entry| self.entry_badge(entry))
    }

    /// Opens `popup` over the current screen, in place of any other popup.
    pub(crate) fn open_popup(&mut self, popup: Popup) {
        if self.view.popup != Some(popup) {
            self.close_popup();
            if popup == Popup::Help {
                self.view.help_scroll = 0;
            }
        }
        self.pointer.selecting = false;
        self.change(|app| &mut app.view.selection, None);
        self.change(|app| &mut app.view.popup, Some(popup));
    }

    /// Closes the open popup and what it had open; focus is where it was before.
    pub(crate) fn close_popup(&mut self) {
        if self.view.popup.take().is_none() {
            return;
        }
        let settings = &mut self.view.settings;
        settings.picker = None;
        settings.field = None;
        settings.drag = None;
        self.redraw();
    }

    /// Opens what a menu entry stands for.
    pub(crate) fn open_entry(&mut self, entry: MenuEntry) {
        match entry {
            // Settings opens on the tab it was last on.
            MenuEntry::Settings => self.open_popup(Popup::Settings),
            MenuEntry::ModelLevels => self.open_settings_tab(Tab::ModelLevels),
            MenuEntry::Routing => self.open_settings_tab(Tab::Routing),
            MenuEntry::Help => self.open_popup(Popup::Help),
        }
    }

    fn open_settings_tab(&mut self, tab: Tab) {
        self.open_popup(Popup::Settings);
        self.show_settings_tab(tab);
    }

    /// The menu button, or `Ctrl`+`O`: opens the menu under the button, or closes it.
    pub(crate) fn toggle_main_menu(&mut self) {
        if self
            .view
            .menu
            .as_ref()
            .is_some_and(|menu| menu.target == crate::view::MenuTarget::Main)
        {
            self.change(|app| &mut app.view.menu, None);
            return;
        }
        // The first entry two rows under the button (the box's border just under it), the box's left
        // border on the frame's edge, like the boxes beneath.
        let at = self
            .drawn
            .hits
            .rect_of(crate::hit::Hit::MenuButton)
            .map_or(Position::new(0, 2), |button| {
                Position::new(button.x.saturating_sub(1), button.y + 2)
            });
        self.open_menu(crate::view::MenuTarget::Main, at);
    }

    /// A key while a popup is open: the popup's own keys first; then `Esc` closes it, `?` opens
    /// Help (or closes it) and `,` opens Settings.
    pub(crate) fn on_popup_key(&mut self, popup: Popup, key: KeyEvent, now: Instant) {
        if popup == Popup::Settings && self.on_settings_key(key, now) {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key.code {
            KeyCode::Esc => self.escape(),
            KeyCode::Char('?') if popup == Popup::Help => self.close_popup(),
            KeyCode::Char('?') => self.open_entry(MenuEntry::Help),
            KeyCode::Char(',') => self.open_entry(MenuEntry::Settings),
            code if popup == Popup::Help => self.on_help_key(code),
            _ => {}
        }
    }

    /// A click on a popup's window, its close button or outside it. Outside it (or on its
    /// border), an open picker or text field in Settings closes first.
    pub(crate) fn popup_click(&mut self, hit: PopupHit) {
        let settings = &self.view.settings;
        let overlay = settings.picker.is_some() || settings.field.is_some();
        match hit {
            PopupHit::Backdrop | PopupHit::Window if overlay => {
                self.view.settings.picker = None;
                self.view.settings.field = None;
                self.redraw();
            }
            PopupHit::Close | PopupHit::Backdrop => self.close_popup(),
            PopupHit::Window => {}
        }
    }
}
