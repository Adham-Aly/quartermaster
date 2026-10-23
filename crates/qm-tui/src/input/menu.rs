//! Context menus and the top bar's menu: their items, drawing them (through
//! `widgets::dropdown`), and running the choice.
//!
//! Menus offer only items that work (open session, stop, the model actions on the Settings
//! board, …).

use crate::app::App;
use crate::hit::{Hit, Hits};
use crate::popups::{self, Anchor, MenuEntry, Placement};
use crate::view::{Menu, MenuTarget};
use crate::widgets::dropdown::{self, Item};
use qm_types::api::{AgentRef, Command, Snapshot};
use qm_types::domain::Autonomy;
use ratatui::Frame;
use ratatui::layout::Position;
use std::time::Instant;

const OPEN_SESSION: &str = "Open session";
const SHOW_TASK: &str = "Show task";
const INTERRUPT: &str = "Interrupt";
const COPY_TASK_ID: &str = "Copy task ID";
const COPY_ID: &str = "Copy ID";
const COPY_NAME: &str = "Copy name";
const OPEN_ACTION: &str = "Open";
const GO_TO: &str = crate::actions::GO_TO;
const COPY_TITLE: &str = "Copy title";
const AUTONOMY_NORMAL: &str = "Autonomy: normal";
const AUTONOMY_AUTO: &str = "Autonomy: auto";

/// The menu item that sets `autonomy`.
const fn autonomy_item(autonomy: Autonomy) -> &'static str {
    match autonomy {
        Autonomy::Normal => AUTONOMY_NORMAL,
        Autonomy::Auto => AUTONOMY_AUTO,
    }
}

/// The autonomy a menu item sets, if it is one of the two.
fn item_autonomy(item: &str) -> Option<Autonomy> {
    Autonomy::ALL
        .iter()
        .copied()
        .find(|&a| autonomy_item(a) == item)
}

/// The items a menu on `target` offers, given the latest snapshot (whether the Quartermaster's
/// turn is running, a project's autonomy).
pub(crate) fn items(target: MenuTarget, snapshot: &Snapshot) -> Vec<&'static str> {
    let turn_running = snapshot.turn_running;
    match target {
        MenuTarget::Crew(AgentRef::Crewmate { .. }) => {
            vec![OPEN_SESSION, SHOW_TASK, INTERRUPT, COPY_TASK_ID]
        }
        MenuTarget::Crew(AgentRef::Secondmate { .. }) => vec![OPEN_SESSION, COPY_ID],
        MenuTarget::Crew(AgentRef::Quartermaster) if turn_running => vec![INTERRUPT],
        MenuTarget::Crew(AgentRef::Quartermaster) => Vec::new(),
        MenuTarget::Project(i) => match snapshot.projects.get(i) {
            Some(_) => vec![AUTONOMY_NORMAL, AUTONOMY_AUTO, COPY_NAME],
            None => Vec::new(),
        },
        MenuTarget::Model(_) | MenuTarget::Account(_) => crate::settings::menu_items(target),
        MenuTarget::Action(i) => match snapshot.actions.get(i) {
            Some(_) => vec![OPEN_ACTION, GO_TO, COPY_TITLE],
            None => Vec::new(),
        },
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
            // The project's current autonomy is marked; choosing it changes nothing.
            MenuTarget::Project(i) => match (item_autonomy(label), app.snapshot.projects.get(i)) {
                (Some(a), Some(project)) if project.autonomy == a => Item::new(label).hint("✓"),
                _ => Item::new(label),
            },
            _ => Item::new(label),
        })
        .collect()
}

/// Draws the open menu over everything beneath it, placed against its anchor in this frame, and
/// records its regions: `Dialog` over the whole box, then one `MenuItem` per row. Returns `false`
/// (drawing nothing) when the row it is anchored to isn't drawn any more.
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, menu: &Menu, hits: &mut Hits) -> bool {
    let rows = rows(app, menu);
    let screen = frame.area();
    let Some(placement) = popups::resolve(menu.anchor, Some(menu.target.row()), hits, screen)
    else {
        return false;
    };
    let at = match placement {
        Placement::At(at) => at,
        Placement::Centred => {
            let size = dropdown::area(&rows, Position::new(0, 1), screen);
            Position::new(
                screen.x + screen.width.saturating_sub(size.width) / 2,
                screen.y + screen.height.saturating_sub(size.height) / 2 + 1,
            )
        }
    };
    let area = dropdown::area(&rows, at, screen);
    dropdown::draw(frame, area, &rows, menu.cursor, hits, Hit::MenuItem);
    true
}

impl App {
    /// Opens the menu on `target` against `anchor`, unless it would be empty.
    pub(crate) fn open_menu(&mut self, target: MenuTarget, anchor: Anchor) {
        let items = items(target, &self.snapshot);
        if items.is_empty() {
            return;
        }
        let menu = Menu {
            target,
            anchor,
            items,
            cursor: 0,
        };
        self.change(|app| &mut app.view.menu, Some(menu));
    }

    /// Selects a row and opens its menu (`Space` from the keyboard), under the row's label.
    pub(crate) fn open_menu_for_row(&mut self, target: MenuTarget) {
        self.select_target(target);
        self.open_menu(target, Anchor::Row { dx: 2, dy: 1 });
    }

    /// The anchor for a menu on `target` opened at the pointer `at` (a right-click): its first
    /// item on the pointer's row, as far into the target's row as the pointer was.
    pub(crate) fn anchor_at(&self, target: MenuTarget, at: Position) -> Anchor {
        let row = self.term.drawn.hits.rect_of(target.row());
        row.map_or(Anchor::Row { dx: 0, dy: 0 }, |row| Anchor::Row {
            dx: at.x.saturating_sub(row.x),
            dy: at.y.saturating_sub(row.y),
        })
    }

    /// Selects the row a menu acts on.
    pub(crate) fn select_target(&mut self, target: MenuTarget) {
        match target {
            MenuTarget::Crew(agent) => self.select_crew(agent),
            MenuTarget::Project(i) => self.change(|app| &mut app.view.selected_project, Some(i)),
            MenuTarget::Action(i) => self.select_action(i),
            MenuTarget::Model(_) | MenuTarget::Account(_) => {
                self.settings_select_target(target);
            }
            MenuTarget::Main => {}
        }
    }

    /// Your autonomy switch for project row `i` (its menu here, or Settings): sent to the core,
    /// which saves it; the next snapshot shows it on the card and in Settings.
    pub(crate) fn set_project_autonomy(&mut self, i: usize, autonomy: Autonomy) {
        if let Some(project) = self.snapshot.projects.get(i).map(|p| p.name.clone()) {
            self.command(Command::SetProjectAutonomy { project, autonomy });
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
            (AUTONOMY_NORMAL | AUTONOMY_AUTO, MenuTarget::Project(i)) => {
                let current = self.snapshot.projects.get(i).map(|p| p.autonomy);
                if let Some(autonomy) = item_autonomy(item)
                    && current != Some(autonomy)
                {
                    self.set_project_autonomy(i, autonomy);
                }
            }
            (OPEN_ACTION, MenuTarget::Action(i)) => self.open_action(i),
            (GO_TO, MenuTarget::Action(i)) => {
                self.open_action(i);
                let go = self
                    .snapshot
                    .actions
                    .get(i)
                    .map(|item| item.card.buttons.len());
                if let Some(go) = go {
                    self.press_action_button(go, now);
                }
            }
            (COPY_TITLE, MenuTarget::Action(i)) => {
                if let Some(title) = self.snapshot.actions.get(i).map(|item| item.title.clone()) {
                    self.copy(&title, now);
                }
            }
            (COPY_NAME, MenuTarget::Project(i)) => {
                if let Some(name) = self.snapshot.projects.get(i).map(|p| p.name.clone()) {
                    self.copy(&name, now);
                }
            }
            (item, MenuTarget::Main) => {
                if let Some(entry) = MenuEntry::from_label(item) {
                    self.choose_entry(entry);
                }
            }
            (item, MenuTarget::Model(_) | MenuTarget::Account(_)) => {
                self.settings_menu_choice(item, menu.target, now);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::api::ProjectRow;
    use qm_types::domain::DeliveryMode;

    fn app_with(autonomy: Autonomy) -> App {
        let mut app = App::new();
        app.snapshot.projects = vec![ProjectRow::new("web-app", DeliveryMode::DirectPr, autonomy)];
        app
    }

    fn sent_autonomy(app: &mut App) -> Vec<(String, Autonomy)> {
        app.take_commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::SetProjectAutonomy { project, autonomy } => Some((project, autonomy)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_projects_menu_offers_both_modes_and_marks_the_current_one() {
        for autonomy in Autonomy::ALL.iter().copied() {
            let mut app = app_with(autonomy);
            assert_eq!(
                items(MenuTarget::Project(0), &app.snapshot),
                [AUTONOMY_NORMAL, AUTONOMY_AUTO, COPY_NAME]
            );
            app.open_menu_for_row(MenuTarget::Project(0));
            let menu = app.view.menu.take().expect("the menu is open");
            let marked: Vec<&str> = rows(&app, &menu)
                .iter()
                .filter(|item| item.hint == "✓")
                .map(|item| item.label)
                .collect();
            assert_eq!(marked, [autonomy_item(autonomy)]);
        }
        let app = app_with(Autonomy::Normal);
        assert_eq!(
            items(MenuTarget::Project(5), &app.snapshot),
            [] as [&str; 0]
        );
    }

    #[test]
    fn choosing_a_mode_sends_the_users_autonomy_command_only_when_it_changes() {
        let mut app = app_with(Autonomy::Normal);
        app.open_menu_for_row(MenuTarget::Project(0));
        app.choose_menu_item(1, Instant::now());
        assert_eq!(
            sent_autonomy(&mut app),
            [("web-app".to_owned(), Autonomy::Auto)]
        );
        let mut app = app_with(Autonomy::Auto);
        app.open_menu_for_row(MenuTarget::Project(0));
        app.choose_menu_item(0, Instant::now());
        assert_eq!(
            sent_autonomy(&mut app),
            [("web-app".to_owned(), Autonomy::Normal)]
        );
        app.open_menu_for_row(MenuTarget::Project(0));
        app.choose_menu_item(1, Instant::now());
        assert!(
            sent_autonomy(&mut app).is_empty(),
            "the current mode changes nothing"
        );
    }
}
