//! The Projects and System tabs. The plan names them without contents. Projects shows your projects
//! and how they deliver, read-only; System shows this Mac, the notifications switch and the entry
//! to the setup check.

use super::paint::Painter;
use super::{SettingsHit, boxed};
use crate::app::App;
use crate::text::{cells, truncate, width};
use crate::theme;
use qm_types::domain::{Autonomy, DeliveryMode};
use ratatui::layout::Rect;

pub(super) fn draw_projects(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let projects = &app.snapshot.projects;
    let rows = cells(projects.len().max(1)) + 2;
    let inner = boxed(p, area, "Projects", rows, theme::dim(), theme::dim());
    let x = inner.x + 1;
    let room = usize::from(inner.width.saturating_sub(2));
    if projects.is_empty() {
        p.text(x, inner.y, "No projects yet.", theme::dim());
    }
    let name_width = projects.iter().map(|r| width(&r.name)).max().unwrap_or(0) + 4;
    for (i, project) in projects.iter().enumerate() {
        let y = inner.y + cells(i);
        p.text(x, y, &truncate(&project.name, room), theme::strong());
        let delivery = match project.delivery {
            DeliveryMode::DirectPr => "pull requests",
            DeliveryMode::BranchOnly => "branches only",
            DeliveryMode::BuildInPlace => "builds in place",
        };
        let autonomy = if project.autonomy == Autonomy::On {
            "autonomy on"
        } else {
            "autonomy off"
        };
        let detail = format!("{delivery} · {autonomy}");
        p.text(
            x + cells(name_width),
            y,
            &truncate(&detail, room.saturating_sub(name_width)),
            theme::dim(),
        );
    }
    let y = inner.y + cells(projects.len().max(1)) + 1;
    p.text(
        x,
        y,
        &truncate(
            "Ask the Quartermaster to add a project or change how it delivers.",
            room,
        ),
        theme::dim(),
    );
}

pub(super) fn draw_system(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let inner = boxed(p, area, "System", 5, theme::dim(), theme::dim());
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
    let col = x + 15;

    // Notifications.
    let y = inner.y + 2;
    p.text(x, y, "Notifications", theme::strong());
    let switch = if app.view.settings.model.notifications {
        "● On"
    } else {
        "○ Off"
    };
    let left = p.button_left_of(right, y, switch, SettingsHit::NotificationsSwitch);
    p.text(
        col,
        y,
        &truncate(
            "bell and desktop notification for questions, decisions and approvals",
            usize::from(left.saturating_sub(col + 1)),
        ),
        theme::text(),
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
}
