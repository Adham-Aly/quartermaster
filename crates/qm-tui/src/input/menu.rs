//! Context menus and the top bar's menu: their items, drawing them (through
//! `widgets::dropdown`), and running the choice.
//!
//! Menus offer only items that work (open session, stop, the model actions on the Settings
//! board, …).

use crate::app::App;
use crate::hit::{Hit, Hits};
use crate::popups::MenuEntry;
use crate::view::{Menu, MenuTarget};
use crate::widgets::dropdown::{self, Item};
use qm_types::api::{AgentRef, Command};
use ratatui::Frame;
use ratatui::layout::Position;
use std::time::Instant;

const OPEN_SESSION: &str = "Open session";
const SHOW_TASK: &str = "Show task";
const INTERRUPT: &str = "Interrupt";
const COPY_TASK_ID: &str = "Copy task ID";
const COPY_ID: &str = "Copy ID";
const COPY_NAME: &str = "Copy name";

/// The items a menu on `target` offers, given whether the Quartermaster's turn is running.
pub(crate) fn items(target: MenuTarget, turn_running: bool) -> Vec<&'static str> {
    match target {
        MenuTarget::Crew(AgentRef::Crewmate { .. }) => {
            vec![OPEN_SESSION, SHOW_TASK, INTERRUPT, COPY_TASK_ID]
        }
        MenuTarget::Crew(AgentRef::Secondmate { .. }) => vec![OPEN_SESSION, COPY_ID],
        MenuTarget::Crew(AgentRef::Quartermaster) if turn_running => vec![INTERRUPT],
        MenuTarget::Crew(AgentRef::Quartermaster) => Vec::new(),
        MenuTarget::Project(_) => vec![COPY_NAME],
        MenuTarget::Model(_) | MenuTarget::AddToTier(_) | MenuTarget::Account(_) => {
            crate::settings::menu_items(target)
        }
        MenuTarget::Main => MenuEntry::ALL.into_iter().map(MenuEntry::label).collect(),
    }
}

/// The rows of `menu` as drawn: the top bar's menu with each entry's key and badge.
fn rows<'a>(app: &App, menu: &'a Menu) -> Vec<Item<'a>> {
    menu.items
        .iter()
        .map(|&label| match menu.target {
            MenuTarget::Main => MenuEntry::from_label(label).map_or(Item::new(label), |entry| {
                Item::new(label)
                    .hint(entry.key())
                    .badge(app.entry_badge(entry))
            }),
            _ => Item::new(label),
        })
        .collect()
}

/// Draws the open menu over everything beneath it, and records its regions: `Dialog` over the
/// whole box, then one `MenuItem` per row.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, menu: &Menu, hits: &mut Hits) {
    let rows = rows(app, menu);
    let area = dropdown::area(&rows, menu.at, frame.area());
    dropdown::draw(frame, area, &rows, menu.cursor, hits, Hit::MenuItem);
}

impl App {
    /// Opens the menu on `target` at `at`, unless it would be empty.
    pub(crate) fn open_menu(&mut self, target: MenuTarget, at: Position) {
        let items = items(target, self.snapshot.turn_running);
        if items.is_empty() {
            return;
        }
        let menu = Menu {
            target,
            at,
            items,
            cursor: 0,
        };
        self.change(|app| &mut app.view.menu, Some(menu));
    }

    /// Selects a row and opens its menu (right-click, or `Space` from the keyboard): at the
    /// pointer for a click, under the row for `Space`.
    pub(crate) fn open_menu_for_row(&mut self, target: MenuTarget, row: Hit) {
        self.select_target(target);
        let at = self
            .drawn
            .hits
            .rect_of(row)
            .map_or(Position::new(0, 0), |area| {
                Position::new(area.x + 2, area.y + 1)
            });
        self.open_menu(target, at);
    }

    /// Selects the row a menu acts on.
    pub(crate) fn select_target(&mut self, target: MenuTarget) {
        match target {
            MenuTarget::Crew(agent) => self.select_crew(agent),
            MenuTarget::Project(i) => self.change(|app| &mut app.view.selected_project, Some(i)),
            MenuTarget::Model(_) | MenuTarget::AddToTier(_) | MenuTarget::Account(_) => {
                self.settings_select_target(target);
            }
            MenuTarget::Main => {}
        }
    }

    /// Moves the menu's highlighted item (keys, and the pointer over an item).
    pub(crate) fn set_menu_cursor(&mut self, cursor: usize) {
        if let Some(menu) = &mut self.view.menu
            && menu.cursor != cursor
            && cursor < menu.items.len()
        {
            menu.cursor = cursor;
            self.redraw();
        }
    }

    /// Runs item `index` of the open menu and closes it.
    pub(crate) fn choose_menu_item(&mut self, index: usize, now: Instant) {
        let Some(menu) = self.view.menu.take() else {
            return;
        };
        self.redraw();
        let Some(&item) = menu.items.get(index) else {
            return;
        };
        match (item, menu.target) {
            (OPEN_SESSION, MenuTarget::Crew(agent)) => self.open_worker_session(agent),
            (SHOW_TASK, MenuTarget::Crew(agent)) => self.open_crew(agent, false, now),
            (INTERRUPT, MenuTarget::Crew(agent)) => self.command(Command::Interrupt { agent }),
            (COPY_TASK_ID, MenuTarget::Crew(AgentRef::Crewmate { task })) => {
                self.copy(&task.to_string(), now);
            }
            (COPY_ID, MenuTarget::Crew(AgentRef::Secondmate { id })) => {
                self.copy(&id.to_string(), now);
            }
            (COPY_NAME, MenuTarget::Project(i)) => {
                if let Some(name) = self.snapshot.projects.get(i).map(|p| p.name.clone()) {
                    self.copy(&name, now);
                }
            }
            (item, MenuTarget::Main) => {
                if let Some(entry) = MenuEntry::from_label(item) {
                    self.open_entry(entry);
                }
            }
            (item, MenuTarget::Model(_) | MenuTarget::AddToTier(_) | MenuTarget::Account(_)) => {
                self.settings_menu_choice(item, menu.target, now);
            }
            _ => {}
        }
    }
}
