//! Popups over the current screen, and the top bar's menu that opens them.
//!
//! Settings, Help and Update open as popup windows over whatever is showing (the Bridge, a
//! session, the setup check, a diff); the screen beneath keeps its focus, so closing a popup
//! (`Esc`, its ` ✕ `, or a click outside it) puts you back where you were. The window itself is
//! `widgets::popup`; the menu's list is `widgets::dropdown`, through the context-menu machinery
//! (`input::menu`, target `MenuTarget::Main`).
//!
//! The open popups are a stack (`View::popups`, top last) of [`OpenPopup`]s: plain, serialisable
//! data with no screen coordinates. Each records its [`Anchor`], what it is placed against (the
//! screen's centre, the menu button, the Projects pane, a row), which is resolved against the
//! frame being drawn and clamped into it ([`resolve`]). So the same stack can be drawn in windows
//! of any size.
//!
//! - A new popup is a [`Popup`] variant, its title and size ([`Popup::title`], [`area`]), a line
//!   in [`draw`], and its keys in [`App::on_popup_key`]. Open it with [`App::open_popup`], or
//!   [`App::open_popup_at`] for an anchor other than the centre.
//! - A new menu entry is a [`MenuEntry`] variant, a place in [`MenuEntry::ALL`], and what choosing
//!   it does in [`App::choose_entry`]: open a popup, or run anything else (a command to the core,
//!   say).
//! - A badge (a red dot that says something there needs you) on an entry: make
//!   [`App::entry_badge`] say so, for instance by putting the entry in `View::badges`. The menu
//!   button shows a dot when any entry does, and an entry's popup shows one in its title.

use crate::app::App;
use crate::hit::{FrameMap, Hit, Hits, Pane};
use crate::settings::Tab;
use crate::text::{cells, width, wrap};
use crate::widgets::popup::{self as window, PopupHit, Window};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// What a popup or a menu is placed against. Never screen cells: [`resolve`] finds it in the
/// frame being drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Anchor {
    /// Centred over the screen's body.
    Centre,
    /// Just under the top bar's menu button.
    MenuButton,
    /// Just under the Projects pane's title, at its left edge.
    ProjectsPane,
    /// On the row a context menu's target names (`MenuTarget::row`), `dx` cells in from its left
    /// edge and `dy` rows down from its top: where the pointer was, or under the row's label.
    Row { dx: u16, dy: u16 },
}

/// Where an anchor puts a popup or menu in the frame being drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Centred.
    Centred,
    /// The box's left edge at `x`, its first row inside the border at `y`.
    At(Position),
}

/// Resolves `anchor` against the regions drawn so far this frame, clamped into `within`; `row` is
/// the region a `Row` anchor names. `None` when that isn't drawn (scrolled away, or gone).
pub(crate) fn resolve(
    anchor: Anchor,
    row: Option<Hit>,
    hits: &Hits,
    within: Rect,
) -> Option<Placement> {
    let at = match anchor {
        Anchor::Centre => return Some(Placement::Centred),
        // The box's border on the row under the button and on the frame's edge, like the boxes
        // beneath; where there is no button (a worker session), at the top left.
        Anchor::MenuButton => hits
            .rect_of(Hit::MenuButton)
            .map_or(Position::new(within.x, within.y + 1), |button| {
                Position::new(button.x.saturating_sub(1), button.bottom() + 1)
            }),
        // The box's border on the row under the pane's title; centred where the pane isn't drawn.
        Anchor::ProjectsPane => {
            let pane = hits.rect_of(Hit::Pane(Pane::Projects));
            return Some(pane.map_or(Placement::Centred, |pane| {
                Placement::At(clamp(Position::new(pane.x, pane.y + 2), within))
            }));
        }
        Anchor::Row { dx, dy } => {
            let row = hits.rect_of(row?)?;
            Position::new(row.x.saturating_add(dx), row.y.saturating_add(dy))
        }
    };
    Some(Placement::At(clamp(at, within)))
}

/// `at`, moved into `within`.
fn clamp(at: Position, within: Rect) -> Position {
    let x =
        at.x.clamp(within.x, within.right().saturating_sub(1).max(within.x));
    let y =
        at.y.clamp(within.y, within.bottom().saturating_sub(1).max(within.y));
    Position::new(x, y)
}

/// An open popup: what it is and what it is placed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OpenPopup {
    pub(crate) popup: Popup,
    pub(crate) anchor: Anchor,
}

/// The widest the Settings popup gets, in cells.
const SETTINGS_WIDTH: u16 = 124;
/// The widest the Help popup gets, in cells.
const HELP_WIDTH: u16 = 104;
/// The widest the projects hint gets, in cells.
const PROJECTS_HINT_WIDTH: u16 = 48;
/// What the projects hint says.
const PROJECTS_HINT: &str =
    "Give your Quartermaster a folder path or a GitHub repo link to add a project.";

/// A window that opens over the current screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Popup {
    /// Settings and accounts, with its tabs.
    Settings,
    /// Keys and mouse help.
    Help,
    /// The app's own update: where it stands, and the button that starts it (`update`).
    Update,
    /// How to add a project: opened from the Projects pane; `Esc` or any click closes it.
    ProjectsHint,
}

impl Popup {
    /// The title in the window's top border.
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Help => "Help",
            Self::Update => "Update",
            Self::ProjectsHint => "Add a project",
        }
    }

    /// The menu entry whose badge the popup's title shows, if it has one.
    const fn entry(self) -> Option<MenuEntry> {
        match self {
            Self::Settings => Some(MenuEntry::Settings),
            Self::Help => Some(MenuEntry::Help),
            Self::Update => Some(MenuEntry::Update),
            Self::ProjectsHint => None,
        }
    }
}

/// An entry in the top bar's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MenuEntry {
    Settings,
    ModelLevels,
    Routing,
    Update,
    Help,
}

impl MenuEntry {
    /// The menu's entries, top to bottom.
    pub(crate) const ALL: [Self; 5] = [
        Self::Settings,
        Self::ModelLevels,
        Self::Routing,
        Self::Update,
        Self::Help,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::ModelLevels => "Model levels",
            Self::Routing => "Routing",
            Self::Update => "Update",
            Self::Help => "Help: keys and mouse",
        }
    }

    /// The key that opens it from anywhere you aren't typing, shown beside it; empty for none.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Settings => ",",
            Self::Help => "?",
            Self::ModelLevels | Self::Routing | Self::Update => "",
        }
    }

    /// The entry labelled `label`.
    pub(crate) fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|entry| entry.label() == label)
    }
}

/// The most `popup` wants, in cells, with `body` (the area between the top bar and the status
/// bar) to show in.
fn size(popup: Popup, body: Rect, app: &App) -> (u16, u16) {
    match popup {
        Popup::Settings => (SETTINGS_WIDTH, body.height),
        Popup::Help => {
            // As tall as its lines at the width it gets (border and padding included).
            let width = window::centered(body, HELP_WIDTH, body.height).width;
            let lines = crate::placeholder::lines(usize::from(width.saturating_sub(4))).len();
            (HELP_WIDTH, u16::try_from(lines + 2).unwrap_or(u16::MAX))
        }
        Popup::Update => {
            // As tall as its lines at the width it gets (border and padding included).
            let width = window::centered(body, crate::update::WIDTH, body.height).width;
            let room = usize::from(width.saturating_sub(4));
            (
                crate::update::WIDTH,
                crate::update::height(&app.snapshot.update, room),
            )
        }
        Popup::ProjectsHint => {
            // Just wide enough for its wrapped lines (border and padding included).
            let room = PROJECTS_HINT_WIDTH.min(body.width).saturating_sub(4);
            let lines = wrap(PROJECTS_HINT, usize::from(room));
            let longest = lines.iter().map(|line| width(line)).max().unwrap_or(0);
            (cells(longest) + 4, cells(lines.len()) + 2)
        }
    }
}

/// Where `open` sits within `body` in this frame, or `None` when its anchor isn't drawn.
fn area(open: OpenPopup, body: Rect, app: &App, hits: &Hits) -> Option<Rect> {
    let (width, height) = size(open.popup, body, app);
    Some(match resolve(open.anchor, None, hits, body)? {
        Placement::Centred => window::centered(body, width, height),
        Placement::At(at) => window::at(body, at, width, height),
    })
}

/// Draws the open popups over `body`, bottom first, and records what the top one holds (each
/// popup's backdrop covers everything beneath it). A popup whose anchor isn't drawn is skipped.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    for &open in &app.view.popups {
        let Some(area) = area(open, body, app, &map.hits) else {
            continue;
        };
        let popup = open.popup;
        let badge = popup.entry().is_some_and(|entry| app.entry_badge(entry));
        let window = Window::new(popup.title()).badge(badge);
        let inner = window::draw(frame, area, window, app.term.hover, map);
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
            Popup::Update => crate::update::draw(frame, content, app, map),
            Popup::ProjectsHint => {
                let lines: Vec<Line<'_>> = wrap(PROJECTS_HINT, usize::from(content.width))
                    .into_iter()
                    .map(Line::raw)
                    .collect();
                frame.render_widget(Paragraph::new(lines), content);
            }
        }
    }
}

impl App {
    /// Whether `entry` shows a badge: something there needs you. The core says when the
    /// update does.
    pub(crate) fn entry_badge(&self, entry: MenuEntry) -> bool {
        self.view.badges.contains(&entry)
            || (entry == MenuEntry::Update && self.snapshot.update.badge)
    }

    /// Whether the menu button shows a badge: some entry does.
    pub(crate) fn menu_badge(&self) -> bool {
        MenuEntry::ALL
            .into_iter()
            .any(|entry| self.entry_badge(entry))
    }

    /// The popup on top, which takes the keys and clicks.
    pub(crate) fn popup(&self) -> Option<Popup> {
        self.view.popups.last().map(|open| open.popup)
    }

    /// Opens `popup` centred over the current screen, on top of any open popup.
    pub(crate) fn open_popup(&mut self, popup: Popup) {
        self.open_popup_at(popup, Anchor::Centre);
    }

    /// Opens `popup` against `anchor`, on top of any open popup; one already open moves to the
    /// top. Settings' picker or text field closes when something covers it.
    pub(crate) fn open_popup_at(&mut self, popup: Popup, anchor: Anchor) {
        let open = OpenPopup { popup, anchor };
        if self.view.popups.last() == Some(&open) {
            return;
        }
        let was_open = self.view.popups.iter().any(|o| o.popup == popup);
        self.view.popups.retain(|o| o.popup != popup);
        if popup == Popup::Help && !was_open {
            self.view.help_scroll = 0;
        }
        if popup != Popup::Settings {
            self.close_settings_overlays();
        }
        self.view.popups.push(open);
        self.term.pointer.selecting = false;
        self.term.selection = None;
        self.redraw();
    }

    /// Closes the popup on top, and what it had open; the one beneath, or the screen with its
    /// focus as it was, shows again.
    pub(crate) fn close_popup(&mut self) {
        if let Some(open) = self.view.popups.pop() {
            if open.popup == Popup::Settings {
                self.close_settings_overlays();
                self.view.settings.drag = None;
            }
            self.redraw();
        }
    }

    /// Closes every popup (going to another screen).
    pub(crate) fn close_popups(&mut self) {
        while !self.view.popups.is_empty() {
            self.close_popup();
        }
    }

    /// Closes Settings' picker and text field.
    fn close_settings_overlays(&mut self) {
        let settings = &mut self.view.settings;
        if settings.picker.is_some() || settings.field.is_some() {
            settings.picker = None;
            settings.field = None;
            self.redraw();
        }
    }

    /// Does what a menu entry stands for.
    pub(crate) fn choose_entry(&mut self, entry: MenuEntry) {
        match entry {
            // Settings opens on the tab it was last on.
            MenuEntry::Settings => self.open_popup(Popup::Settings),
            MenuEntry::ModelLevels => self.open_settings_tab(Tab::ModelLevels),
            MenuEntry::Routing => self.open_settings_tab(Tab::Routing),
            MenuEntry::Update => self.open_popup(Popup::Update),
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
        self.open_menu(crate::view::MenuTarget::Main, Anchor::MenuButton);
    }

    /// The Projects pane's hint on how to add a project, under the pane's title.
    pub(crate) fn open_projects_hint(&mut self) {
        self.open_popup_at(Popup::ProjectsHint, Anchor::ProjectsPane);
    }

    /// A key while a popup is open: the popup's own keys first; then `Esc` closes it, `?` opens
    /// Help (or closes it) and `,` opens Settings.
    pub(crate) fn on_popup_key(&mut self, popup: Popup, key: KeyEvent, now: Instant) {
        if popup == Popup::Settings && self.on_settings_key(key, now) {
            return;
        }
        if popup == Popup::Update && self.on_update_key(key.code) {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        match key.code {
            KeyCode::Esc => self.escape(),
            KeyCode::Char('?') if popup == Popup::Help => self.close_popup(),
            KeyCode::Char('?') => self.choose_entry(MenuEntry::Help),
            KeyCode::Char(',') => self.choose_entry(MenuEntry::Settings),
            code if popup == Popup::Help => self.on_help_key(code),
            _ => {}
        }
    }

    /// A click on the top popup's window, its close button or outside it. Outside it (or on its
    /// border), an open picker or text field in Settings closes first. Any click closes the
    /// projects hint.
    pub(crate) fn popup_click(&mut self, hit: PopupHit) {
        let settings = &self.view.settings;
        let overlay = self.popup() == Some(Popup::Settings)
            && (settings.picker.is_some() || settings.field.is_some());
        match hit {
            PopupHit::Backdrop | PopupHit::Window if overlay => self.close_settings_overlays(),
            PopupHit::Close | PopupHit::Backdrop => self.close_popup(),
            PopupHit::Window if self.popup() == Some(Popup::ProjectsHint) => self.close_popup(),
            PopupHit::Window => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_hint_fits_its_text() {
        let (width, height) = size(Popup::ProjectsHint, Rect::new(0, 1, 150, 42), &App::new());
        assert!(width <= PROJECTS_HINT_WIDTH);
        let lines = wrap(PROJECTS_HINT, usize::from(width - 4));
        assert_eq!(cells(lines.len()) + 2, height);
        assert_eq!(
            lines.concat().replace(' ', ""),
            PROJECTS_HINT.replace(' ', "")
        );
    }

    #[test]
    fn projects_hint_opens_under_the_pane_title() {
        let body = Rect::new(0, 1, 150, 42);
        let mut hits = Hits::default();
        hits.add(Rect::new(0, 1, 31, 6), Hit::Pane(Pane::Projects));
        assert_eq!(
            resolve(Anchor::ProjectsPane, None, &hits, body),
            Some(Placement::At(Position::new(0, 3))),
        );
        // Where the pane isn't drawn, it opens centred.
        assert_eq!(
            resolve(Anchor::ProjectsPane, None, &Hits::default(), body),
            Some(Placement::Centred),
        );
    }
}
