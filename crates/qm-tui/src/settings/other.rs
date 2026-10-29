//! The Projects and System tabs. Projects shows your projects and how they deliver, with each
//! one's autonomy, normal or auto (yours alone: no agent can change it); System shows this Mac, the
//! notifications switch and the entry to the setup check, with what it found wrong and how to fix
//! it.

use super::paint::Painter;
use super::{SettingsHit, section};
use crate::app::App;
use crate::text::{cells, truncate, width};
use crate::theme;
use qm_types::api::SetupMark;
use qm_types::attention::Health;
use qm_types::domain::{Autonomy, DeliveryMode};
use ratatui::layout::Rect;
use ratatui::style::Style;

pub(super) fn draw_projects(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let projects = &app.snapshot.projects;
    let rows = cells(projects.len().max(1)) + 4;
    let inner = section(area, rows);
    let x = inner.x + 1;
    let room = usize::from(inner.width.saturating_sub(2));
    if projects.is_empty() {
        p.text(x, inner.y, "No projects yet.", theme::dim());
    }
    // Wide enough for the longest project name.
    let name_width = projects.iter().map(|r| width(&r.name)).max().unwrap_or(0) + 4;
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
    let y = inner.y + cells(projects.len().max(1)) + 1;
    p.text(
        x,
        y,
        &truncate(
            "Projects appear when work starts in a folder. Ask the Quartermaster to change how one delivers. Only you set autonomy: normal asks you to approve plans, review findings and merges; auto doesn't.",
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
    let inner = section(area, 5 + cells(findings.len()));
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

    // Notifications.
    let y = inner.y + 2;
    p.text(x, y, "Notifications", theme::strong());
    let switch = if app.view.settings.model.notifications {
        "● On"
    } else {
        "○ Off"
    };
    let left = p.button_left_of(right, y, switch, SettingsHit::NotificationsSwitch);
    // The text stops where the wider of the two would start, so it holds still as you flip it.
    let wider = u16::try_from(width("○ Off") - width(switch)).unwrap_or(0);
    let (about, style) = notifications_about(
        app.view.settings.model.notifications,
        app.view.settings.notifier,
    );
    p.text(
        col,
        y,
        &truncate(
            about,
            usize::from(left.saturating_sub(wider).saturating_sub(col + 1)),
        ),
        style,
    );

    // The setup check.
    let y = inner.y + 4;
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

/// The Notifications row's text: what they do, or what stops them while they are on.
fn notifications_about(on: bool, health: Health) -> (&'static str, Style) {
    let problem = match health {
        _ if !on => None,
        Health::TextHidden => Some("text hidden: set Show previews to Always"),
        Health::Denied => Some("blocked: allow them in System Settings"),
        Health::Failed => Some("can't start: the log says why"),
        _ => None,
    };
    problem.map_or(
        (
            "a banner and the menu bar mark when something waits on you",
            theme::text(),
        ),
        |problem| (problem, theme::warn()),
    )
}

/// A mode's button text: `● normal` when it is the project's, `○ auto` when it isn't.
fn mode_label(mode: Autonomy, current: bool) -> String {
    format!("{} {mode}", if current { "●" } else { "○" })
}
