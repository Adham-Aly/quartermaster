//! Context menus: their items, drawing them, and running the choice.
//!
//! Menus offer only items that work (open session, stop, the model actions on the Settings
//! board, …).

use crate::app::App;
use crate::hit::{Hit, Hits};
use crate::text;
use crate::theme;
use crate::view::{Menu, MenuTarget};
use qm_types::api::{AgentRef, Command};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
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
    }
}

/// Where a menu of `items` opened at `at` sits: right of the pointer with its first item on the
/// pointer's row, shifted to stay inside `screen`.
pub(crate) fn area(items: &[&str], at: Position, screen: Rect) -> Rect {
    let widest = items
        .iter()
        .map(|item| text::width(item))
        .max()
        .unwrap_or(0);
    // A border and one cell of padding on each side.
    let width = text::cells(widest + 4).min(screen.width);
    let height = text::cells(items.len() + 2).min(screen.height);
    let x = at.x.min(screen.right().saturating_sub(width)).max(screen.x);
    let y =
        at.y.saturating_sub(1)
            .min(screen.bottom().saturating_sub(height))
            .max(screen.y);
    Rect::new(x, y, width, height)
}

/// Draws the open menu over everything beneath it, and records its regions: `Dialog` over the
/// whole box, then one `MenuItem` per row.
pub(crate) fn draw(frame: &mut Frame<'_>, menu: &Menu, hits: &mut Hits) {
    let area = area(&menu.items, menu.at, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    hits.add(area, Hit::Dialog);
    let inner_width = usize::from(inner.width);
    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let label = text::truncate(&format!(" {item}"), inner_width);
            let pad = inner_width.saturating_sub(text::width(&label));
            let style = if i == menu.cursor {
                theme::text().add_modifier(Modifier::REVERSED)
            } else {
                theme::text()
            };
            Line::from(Span::styled(format!("{label}{}", " ".repeat(pad)), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    for i in 0..menu.items.len() {
        let Ok(row) = u16::try_from(i) else { break };
        if row >= inner.height {
            break;
        }
        hits.add(
            Rect::new(inner.x, inner.y + row, inner.width, 1),
            Hit::MenuItem(i),
        );
    }
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
            (item, MenuTarget::Model(_) | MenuTarget::AddToTier(_) | MenuTarget::Account(_)) => {
                self.settings_menu_choice(item, menu.target, now);
            }
            _ => {}
        }
    }
}
