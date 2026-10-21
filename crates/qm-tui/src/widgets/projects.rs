//! The projects pane: `●` accent when active, `○` dim otherwise, the name, then the delivery mode
//! and autonomy dim, aligned on the longest name. Right-click (or `Space`) a row for its menu,
//! which switches its autonomy.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width};
use crate::theme;
use qm_types::api::ProjectRow;
use qm_types::domain::{Autonomy, DeliveryMode};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Draws the projects box into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let block = pane_block("Projects", pane_style(app, Pane::Projects));
    let rows = rows(app, block.inner(area).width);
    draw_list(
        frame,
        area,
        block,
        rows,
        app.view.projects_scroll,
        Pane::Projects,
        map,
    );
}

/// The pane's rows at `inner_width`; their count sizes the box.
pub(crate) fn rows(app: &App, inner_width: u16) -> Vec<ListRow> {
    let projects = &app.snapshot.projects;
    if projects.is_empty() {
        return empty_lines(
            &["No projects yet.", "Ask the Quartermaster to add one."],
            inner_width,
        )
        .into_iter()
        .map(ListRow::plain)
        .collect();
    }
    let name_width = projects.iter().map(|p| width(&p.name)).max().unwrap_or(0);
    projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let hit = Hit::Project(i);
            let selected =
                (app.view.selected_project == Some(i)).then_some(!app.view.pointer_picked);
            let hovered = app.term.hover == Some(hit);
            ListRow {
                line: line(p, name_width, inner_width, hovered),
                hit: Some(hit),
                selected,
            }
        })
        .collect()
}

/// One project: `● api-server  PR · auto off` (or `branch · auto on`, `in place · auto off`).
/// The name is cut with `…` when the row is narrower than `inner_width`, so the status fits.
fn line(project: &ProjectRow, name_width: usize, inner_width: u16, hovered: bool) -> Line<'static> {
    let (dot, dot_style) = if project.active {
        ("●", theme::accent())
    } else {
        ("○", theme::dim())
    };
    let mode = match project.delivery {
        DeliveryMode::DirectPr => "PR",
        DeliveryMode::BranchOnly => "branch",
        DeliveryMode::BuildInPlace => "in place",
    };
    let auto = match project.autonomy {
        Autonomy::Off => "auto off",
        Autonomy::On => "auto on",
    };
    let delivery = format!("{mode} · {auto}");
    // The dot and its space, then two cells before the status.
    let room = usize::from(inner_width).saturating_sub(width(&delivery) + 4);
    let name = truncate(&project.name, room.max(1));
    let pad = name_width.min(room).saturating_sub(width(&name)) + 2;
    let name_style = if hovered {
        theme::text().add_modifier(Modifier::UNDERLINED)
    } else {
        theme::text()
    };
    Line::from(vec![
        Span::styled(dot, dot_style),
        Span::raw(" "),
        Span::styled(name, name_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(delivery, theme::dim()),
    ])
}
