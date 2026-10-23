//! The Projects and System tabs. Projects shows your projects and how they deliver, with each
//! one's autonomy, normal or auto (yours alone: no agent can change it); System shows this Mac and
//! the entry to the setup check, with what it found wrong and how to fix it.

use super::paint::Painter;
use super::{SettingsHit, section};
use crate::app::App;
use crate::text::{cells, truncate, width};
use crate::theme;
use qm_types::api::SetupMark;
use qm_types::domain::{Autonomy, DeliveryMode};
use ratatui::layout::Rect;
use ratatui::style::Style;

const PERSONAL_FOLDER_LABEL: &str = "Personal folder";

pub(super) fn draw_projects(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let projects = &app.snapshot.projects;
    let rows = cells(projects.len().max(1)) + 4;
    let inner = section(area, rows);
    let x = inner.x + 1;
    let room = usize::from(inner.width.saturating_sub(2));
    if projects.is_empty() {
        p.text(x, inner.y, "No projects yet.", theme::dim());
    }
    // Wide enough for the longest project name and for the "Personal folder" label below it.
    let name_width = projects
        .iter()
        .map(|r| width(&r.name))
        .max()
        .unwrap_or(0)
        .max(width(PERSONAL_FOLDER_LABEL))
        + 4;
    let right = inner.right().saturating_sub(1);
    let selected = app
        .view
        .settings
        .project_row
        .min(projects.len().saturating_sub(1));
    for (i, project) in projects.iter().enumerate() {
        let y = inner.y + cells(i);
        // `autonomy  ● normal  ○ auto`: one button per mode, the current one filled.
        let mut left = right;
        for &mode in Autonomy::ALL.iter().rev() {
            let label = mode_label(mode, project.autonomy == mode);
            left = p.button_left_of(left, y, &label, SettingsHit::ProjectAutonomy(i, mode));
        }
        let label = p.text_left_of(left.saturating_sub(1), y, "autonomy", theme::text());
        let room = usize::from(label.saturating_sub(x + 2));
        p.text(x, y, &truncate(&project.name, room), theme::strong());
        let delivery = match project.delivery {
            DeliveryMode::DirectPr => "pull requests",
            DeliveryMode::BranchOnly => "branches only",
            DeliveryMode::BuildInPlace => "builds in place",
            DeliveryMode::Folder => "a plain folder",
        };
        p.text(
            x + cells(name_width),
            y,
            &truncate(delivery, room.saturating_sub(name_width)),
            theme::dim(),
        );
        if i == selected {
            p.select_row(inner, y);
        }
    }
    // The personal folder: where personal tasks work, changed with f.
    let folder_y = inner.y + cells(projects.len().max(1)) + 1;
    let folder = &app.view.settings.model.workspace_folder;
    p.text(x, folder_y, PERSONAL_FOLDER_LABEL, theme::strong());
    let folder = if folder.is_empty() {
        "—"
    } else {
        folder.as_str()
    };
    p.text(
        x + cells(name_width),
        folder_y,
        &truncate(folder, room.saturating_sub(name_width)),
        theme::dim(),
    );
    let y = folder_y + 2;
    p.text(
        x,
        y,
        &truncate(
            "Ask the Quartermaster to add a project or change how it delivers. Only you set autonomy: normal asks you to approve plans and merges; auto doesn't.",
            room,
        ),
        theme::dim(),
    );
}

/// Where the setup check's findings start, from the section's left edge.
const SYSTEM_COLUMN: u16 = 16;

/// The rows under the setup check's row, `room` cells wide: for each tool that needs you, its mark
/// and what's wrong, then how to fix it, wrapped and dim.
fn setup_findings(app: &App, room: usize) -> Vec<Vec<(String, Style)>> {
    let mut rows = Vec::new();
    for item in app
        .snapshot
        .setup
        .items
        .iter()
        .filter(|i| i.mark != SetupMark::Ok)
    {
        let mark = if item.mark == SetupMark::Missing {
            ("✗ ".to_owned(), theme::error())
        } else {
            ("! ".to_owned(), theme::warn())
        };
        rows.push(vec![
            mark,
            (
                truncate(&item.summary, room.saturating_sub(2)),
                theme::text(),
            ),
        ]);
        if let Some(fix) = &item.fix {
            for line in crate::text::wrap(fix, room.saturating_sub(2).max(1)) {
                rows.push(vec![("  ".to_owned(), theme::text()), (line, theme::dim())]);
            }
        }
    }
    rows
}

pub(super) fn draw_system(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let room = usize::from(area.width.saturating_sub(SYSTEM_COLUMN + 1));
    let findings = setup_findings(app, room);
    let inner = section(area, 3 + cells(findings.len()));
    let x = inner.x + 1;
    let room = usize::from(inner.width.saturating_sub(2));
    let machine = if app.snapshot.machine.is_empty() {
        "unknown"
    } else {
        app.snapshot.machine.as_str()
    };
    let end = p.text(x, inner.y, "This Mac", theme::strong());
    p.text(
        end + 4,
        inner.y,
        &truncate(machine, room.saturating_sub(12)),
        theme::text(),
    );
    let right = inner.right().saturating_sub(1);
    let col = inner.x + SYSTEM_COLUMN;

    // The setup check.
    let y = inner.y + 2;
    p.text(x, y, "Setup check", theme::strong());
    let left = p.button_left_of(right, y, "Open", SettingsHit::SetupCheck);
    p.text(
        col,
        y,
        &truncate(
            "the tools this Mac needs: Claude Code and the GitHub CLI",
            usize::from(left.saturating_sub(col + 1)),
        ),
        theme::text(),
    );
    for (row, runs) in (y + 1..).zip(findings) {
        let mut at = col;
        for (text, style) in runs {
            at = p.text(at, row, &text, style);
        }
    }
}

/// A mode's button text: `● normal` when it is the project's, `○ auto` when it isn't.
fn mode_label(mode: Autonomy, current: bool) -> String {
    format!("{} {mode}", if current { "●" } else { "○" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hit::{Hit, Hits};
    use crate::popups::Popup;
    use crate::router::Screen;
    use crate::settings::Tab;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use qm_types::api::{Command, ProjectRow};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    fn app() -> App {
        let mut app = App::new();
        app.view.settings.loaded = true;
        app.view.settings.tab = Tab::Projects;
        app.snapshot.projects = vec![
            ProjectRow::new("api-server", DeliveryMode::DirectPr, Autonomy::Normal),
            ProjectRow::new("web-app", DeliveryMode::DirectPr, Autonomy::Auto),
        ];
        app
    }

    fn sent(app: &mut App) -> Vec<(String, Autonomy)> {
        app.take_commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::SetProjectAutonomy { project, autonomy } => Some((project, autonomy)),
                _ => None,
            })
            .collect()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_settings_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
    }

    #[test]
    fn system_setup_check_opens_by_key() {
        let mut app = App::new();
        app.open_popup(Popup::Settings);
        let now = Instant::now();
        app.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE), now);
        assert_eq!(app.view.settings.tab, Tab::System);
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), now);
        assert_eq!(app.screen, Screen::Setup);
        assert_eq!(app.popup(), None);
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }

    #[test]
    fn former_system_shortcuts_do_not_change_settings_or_reach_the_prompt() {
        let mut app = App::new();
        app.view.settings.tab = Tab::System;
        app.open_popup(Popup::Settings);
        let before = app.view.settings.model.clone();
        for code in [KeyCode::Char('n'), KeyCode::Char(' ')] {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
            assert_eq!(app.view.settings.model, before);
            assert_eq!(
                app.view.settings.pending,
                [] as [super::super::sync::Pending; 0]
            );
            assert_eq!(app.take_commands(), [] as [Command; 0]);
            assert_eq!(app.screen, Screen::Bridge);
            assert_eq!(app.popup(), Some(Popup::Settings));
            assert_eq!(app.draft.text, "");
        }
    }

    #[test]
    fn compact_system_setup_check_draws_and_opens_by_click_at_supported_sizes() {
        for (width, height) in [(150, 45), (120, 40), (119, 40), (100, 30)] {
            let mut app = App::new();
            app.view.settings.tab = Tab::System;
            app.open_popup(Popup::Settings);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let open = app
                .term
                .drawn
                .hits
                .rect_of(Hit::Settings(SettingsHit::SetupCheck))
                .expect("System draws a reachable Setup check button");
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..height)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            let machine_y = rows
                .iter()
                .position(|row| row.contains("This Mac"))
                .unwrap();
            assert_eq!(usize::from(open.y), machine_y + 2);
            assert!(rows[usize::from(open.y)].contains("Setup check"));
            assert!(rows[usize::from(open.y)].contains("Open"));
            assert!(!rows.iter().any(|row| row.contains("Notifications")));
            app.on_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: open.x,
                    row: open.y,
                    modifiers: KeyModifiers::NONE,
                },
                Instant::now(),
            );
            assert_eq!(app.screen, Screen::Setup);
            assert_eq!(app.popup(), None);
            assert_eq!(app.take_commands(), [] as [Command; 0]);
        }
    }

    #[test]
    fn keys_select_a_project_and_flip_its_autonomy() {
        let mut app = app();
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.view.settings.project_row, 1);
        assert_eq!(
            sent(&mut app),
            [
                ("api-server".to_owned(), Autonomy::Auto),
                ("web-app".to_owned(), Autonomy::Normal),
            ]
        );
    }

    #[test]
    fn each_project_row_draws_both_modes_and_a_click_sets_one() {
        let mut app = app();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut hits = Hits::default();
        let area = Rect::new(0, 0, 100, 20);
        terminal
            .draw(|frame| {
                let mut p = Painter::new(frame, &mut hits, None, true, area);
                draw_projects(&mut p, area, &app);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row = |y: u16| -> String { (0..100).map(|x| buffer[(x, y)].symbol()).collect() };
        let normal = hits
            .rect_of(Hit::Settings(SettingsHit::ProjectAutonomy(
                0,
                Autonomy::Normal,
            )))
            .expect("normal on the first row");
        let auto = hits
            .rect_of(Hit::Settings(SettingsHit::ProjectAutonomy(
                1,
                Autonomy::Auto,
            )))
            .expect("auto on the second row");
        assert!(
            row(normal.y).contains("autonomy  ● normal  ○ auto "),
            "{}",
            row(normal.y)
        );
        assert!(
            row(auto.y).contains("autonomy  ○ normal  ● auto "),
            "{}",
            row(auto.y)
        );
        app.settings_click(
            SettingsHit::ProjectAutonomy(1, Autonomy::Normal),
            false,
            Instant::now(),
        );
        app.settings_click(
            SettingsHit::ProjectAutonomy(0, Autonomy::Normal),
            false,
            Instant::now(),
        );
        assert_eq!(
            sent(&mut app),
            [("web-app".to_owned(), Autonomy::Normal)],
            "the current mode changes nothing"
        );
    }
}
