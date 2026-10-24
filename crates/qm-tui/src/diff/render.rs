//! Cell-based split layout. All geometry and scroll limits are recorded in the frame map.

use super::rows::{self, Row};
use super::tree::{self, status_label};
use super::{App, DiffHit, FrameMap, Hit};
use crate::text::{cells, truncate, width};
use crate::theme;
use qm_types::api::{DiffFile, DiffFileStatus, DiffLine, DiffLineKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthChar;

pub(super) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    if body.height < 6 || body.width < 30 {
        return;
    }
    let view = app.diff_view();
    let back = Rect::new(body.right().saturating_sub(9), body.y, 8, 1);
    let title = view.map_or("Reading the diff…", |v| v.title.as_str());
    frame.render_widget(
        Line::from(vec![
            Span::styled(" Diff · ", theme::accent()),
            Span::styled(
                truncate(&visible(title), usize::from(body.width.saturating_sub(18))),
                theme::strong(),
            ),
        ]),
        Rect::new(body.x, body.y, body.width.saturating_sub(10), 1),
    );
    frame.render_widget(
        Span::styled(
            " ← Back ",
            theme::button(app.term.hover == Some(Hit::Diff(DiffHit::Back))),
        ),
        back,
    );
    map.hits.add(back, Hit::Diff(DiffHit::Back));
    let heading_rows = 1 + draw_summary(frame, body, view);
    let tree_width = (body.width / 4).clamp(20, 36);
    let tree_area = Rect::new(
        body.x,
        body.y + heading_rows,
        tree_width,
        body.height - heading_rows,
    );
    let code_area = Rect::new(
        tree_area.right() + 1,
        tree_area.y,
        body.width - tree_width - 1,
        tree_area.height,
    );
    let files: &[DiffFile] = view.map_or(&[], |v| v.files.as_slice());
    draw_tree(frame, tree_area, app, files, map);
    if let Some(file) = files.get(app.view.diff.file.min(files.len().saturating_sub(1))) {
        draw_file(frame, code_area, app, file, files.len(), map);
    } else {
        let block = panel(" Changes ", !app.view.diff.tree_focus);
        let inner = block.inner(code_area);
        frame.render_widget(block, code_area);
        map.hits.add(inner, Hit::Diff(DiffHit::Body));
        let message = if let Some(view) = view {
            if view.lines.is_empty() {
                vec![Line::from(Span::styled(
                    "No changes in this diff.",
                    theme::dim(),
                ))]
            } else {
                view.lines
                    .iter()
                    .map(|s| Line::from(Span::styled(visible(&s.text), theme::tone(s.tone))))
                    .collect()
            }
        } else {
            vec![Line::from(Span::styled("Reading the diff…", theme::dim()))]
        };
        map.diff_scroll_max = message.len().saturating_sub(usize::from(inner.height));
        frame.render_widget(
            Paragraph::new(message)
                .scroll((cells(app.view.diff.scroll.min(map.diff_scroll_max)), 0)),
            inner,
        );
    }
}

/// Wrap the complete badge key onto its own row rather than dropping a file status.
fn draw_summary(frame: &mut Frame<'_>, body: Rect, view: Option<&qm_types::api::DiffView>) -> u16 {
    let mut summary = Line::default();
    let Some(view) = view else {
        return 1;
    };
    if view.truncated {
        summary.push_span(Span::styled(
            " Partial diff · patch cut/incomplete · ",
            theme::warn(),
        ));
    }
    summary.push_span(Span::styled(format!(" {}", view.stat), theme::strong()));
    let mut legend = Line::default();
    for (label, style) in [
        ("A added", theme::ok()),
        ("M modified", theme::accent()),
        ("D deleted", theme::error()),
        ("R renamed", theme::blue()),
        ("C copied", theme::blue()),
    ] {
        if !legend.spans.is_empty() {
            legend.push_span(Span::styled(" · ", theme::dim()));
        }
        legend.push_span(Span::styled(label, style));
    }
    let wrap = summary.width() + legend.width() + 3 > usize::from(body.width);
    if wrap {
        frame.render_widget(legend, Rect::new(body.x + 1, body.y + 2, body.width - 1, 1));
    } else {
        summary.push_span(Span::styled(" · ", theme::dim()));
        summary.spans.extend(legend.spans);
    }
    frame.render_widget(summary, Rect::new(body.x, body.y + 1, body.width, 1));
    if wrap { 2 } else { 1 }
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border(focused, false))
        .title(title)
}

fn status_style(status: DiffFileStatus) -> Style {
    match status {
        DiffFileStatus::Added => theme::ok(),
        DiffFileStatus::Deleted => theme::error(),
        DiffFileStatus::Renamed | DiffFileStatus::Copied => theme::blue(),
        _ => theme::accent(),
    }
}

fn draw_tree(frame: &mut Frame<'_>, area: Rect, app: &App, files: &[DiffFile], map: &mut FrameMap) {
    let entries = tree::entries(files);
    let block = panel(
        format!(" Files · {} ", files.len()),
        app.view.diff.tree_focus,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(inner, Hit::Diff(DiffHit::Tree));
    let height = usize::from(inner.height);
    map.diff_tree_scroll_max = entries.len().saturating_sub(height);
    let scroll = app.view.diff.tree_scroll.min(map.diff_tree_scroll_max);
    for (i, entry) in entries.iter().skip(scroll).take(height).enumerate() {
        let row = Rect::new(inner.x, inner.y + cells(i), inner.width, 1);
        let selected = entry.file == Some(app.view.diff.file);
        let hover = entry
            .file
            .is_some_and(|i| app.term.hover == Some(Hit::Diff(DiffHit::File(i))));
        let filled = selected && app.view.diff.tree_focus && !app.view.pointer_picked;
        let base = if filled {
            theme::filled()
        } else {
            theme::text()
        };
        frame.render_widget(Block::new().style(base), row);
        let indent = "  ".repeat(entry.depth);
        let marker = if selected {
            "▸"
        } else if entry.file.is_none() {
            "▾"
        } else {
            " "
        };
        let prefix = format!("{marker} {indent}");
        let (badge, _) = status_label(entry.status);
        let room = usize::from(inner.width).saturating_sub(width(&prefix) + 3);
        let label = truncate(&visible(&entry.label), room);
        let name_style = if entry.file.is_none() {
            theme::dim()
        } else if hover {
            theme::strong()
        } else {
            base
        };
        let badge_style = status_style(entry.status);
        frame.render_widget(
            Line::from(vec![
                Span::styled(prefix, if filled { base } else { theme::accent() }),
                Span::styled(
                    label,
                    if filled {
                        theme::on_selection(name_style)
                    } else {
                        name_style
                    },
                ),
            ]),
            row,
        );
        frame.render_widget(
            Span::styled(
                badge,
                if filled {
                    theme::on_selection(badge_style)
                } else {
                    badge_style
                },
            ),
            Rect::new(row.right().saturating_sub(2), row.y, 1, 1),
        );
        if let Some(index) = entry.file {
            map.hits.add(row, Hit::Diff(DiffHit::File(index)));
        }
    }
    if scroll > 0 {
        frame.render_widget(
            Span::styled(" ↑ ", theme::accent()),
            Rect::new(area.right() - 4, area.y, 3, 1),
        );
    }
    if scroll < map.diff_tree_scroll_max {
        frame.render_widget(
            Span::styled(" ↓ ", theme::accent()),
            Rect::new(area.right() - 4, area.bottom() - 1, 3, 1),
        );
    }
}

fn draw_file(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    file: &DiffFile,
    count: usize,
    map: &mut FrameMap,
) {
    let (badge, label) = status_label(file.status);
    let index = app
        .diff_file_order()
        .iter()
        .position(|&i| i == app.view.diff.file)
        .unwrap_or(0);
    let suffix = format!(" · {}/{count} ", index + 1);
    let path = truncate(
        &visible(file.path().unwrap_or("(unknown path)")),
        usize::from(area.width).saturating_sub(width(&suffix) + 6),
    );
    let block = panel(
        Line::from(vec![
            Span::styled(format!(" {badge} "), status_style(file.status)),
            Span::styled(path, theme::strong()),
            Span::styled(suffix, theme::dim()),
        ]),
        !app.view.diff.tree_focus,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let additions = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == DiffLineKind::Addition)
        .count();
    let deletions = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == DiffLineKind::Deletion)
        .count();
    frame.render_widget(
        Line::from(vec![
            Span::styled(format!(" {label}  "), status_style(file.status)),
            Span::styled(format!("+{additions}"), theme::ok()),
            Span::styled(format!("  −{deletions}"), theme::error()),
        ]),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let split = inner.width / 2;
    let left = Rect::new(inner.x, inner.y + 1, split, 1);
    let right = Rect::new(
        left.right() + 1,
        left.y,
        inner.width.saturating_sub(split + 1),
        1,
    );
    for (rect, name, path, style) in [
        (left, "Before", file.old_path.as_deref(), theme::error()),
        (right, "After", file.new_path.as_deref(), theme::ok()),
    ] {
        let label = format!(" {name} · {}", visible(path.unwrap_or("(absent)")));
        frame.render_widget(
            Span::styled(
                truncate(&label, usize::from(rect.width)),
                style.add_modifier(Modifier::BOLD),
            ),
            rect,
        );
    }
    let content = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);
    map.hits.add(content, Hit::Diff(DiffHit::Body));
    draw_code(frame, area, content, app, file, map);
}

fn draw_code(
    frame: &mut Frame<'_>,
    area: Rect,
    content: Rect,
    app: &App,
    file: &DiffFile,
    map: &mut FrameMap,
) {
    let rows = rows::rows(file, app.diff_view().is_some_and(|v| v.truncated));
    map.diff_scroll_max = rows.len().saturating_sub(usize::from(content.height));
    let scroll = app.view.diff.scroll.min(map.diff_scroll_max);
    let (gutter, horizontal_max) = code_dimensions(file, content.width);
    map.diff_horizontal_max = horizontal_max;
    let horizontal = app.view.diff.horizontal.min(map.diff_horizontal_max);
    for (i, row) in rows
        .iter()
        .skip(scroll)
        .take(usize::from(content.height))
        .enumerate()
    {
        let area = Rect::new(content.x, content.y + cells(i), content.width, 1);
        draw_row(frame, area, row, gutter, horizontal, map);
    }
    let end = (scroll + usize::from(content.height)).min(rows.len());
    let footer = format!(
        " {}–{end}/{} rows · col {} ",
        scroll + 1,
        rows.len(),
        horizontal + 1
    );
    frame.render_widget(
        Span::styled(
            truncate(&footer, usize::from(area.width.saturating_sub(4))),
            theme::dim(),
        ),
        Rect::new(
            area.x + 2,
            area.bottom() - 1,
            area.width.saturating_sub(4),
            1,
        ),
    );
    for (show, y, arrow) in [
        (scroll > 0, area.y, " ↑ "),
        (scroll < map.diff_scroll_max, area.bottom() - 1, " ↓ "),
    ] {
        if show {
            frame.render_widget(
                Span::styled(arrow, theme::accent()),
                Rect::new(area.right() - 4, y, 3, 1),
            );
        }
    }
}

/// Reuse the last measured viewport with the newly selected file, even before a redraw.
pub(super) fn code_limits(file: &DiffFile, viewport: Rect, truncated: bool) -> (usize, usize) {
    (
        rows::rows(file, truncated)
            .len()
            .saturating_sub(usize::from(viewport.height)),
        code_dimensions(file, viewport.width).1,
    )
}

fn code_dimensions(file: &DiffFile, width: u16) -> (u16, usize) {
    let side_width = width.saturating_sub(1) / 2;
    let digits = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .flat_map(|l| [l.old_number, l.new_number])
        .flatten()
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(3);
    let gutter = cells(digits + 3).min(side_width.saturating_sub(4));
    let room = usize::from(side_width.saturating_sub(gutter));
    let horizontal = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| crate::text::width(&visible(&l.text)).saturating_sub(room))
        .max()
        .unwrap_or(0);
    (gutter, horizontal)
}

fn draw_row(
    frame: &mut Frame<'_>,
    area: Rect,
    row: &Row<'_>,
    gutter: u16,
    horizontal: usize,
    map: &mut FrameMap,
) {
    let left = Rect::new(area.x, area.y, area.width / 2, 1);
    let right = Rect::new(left.right() + 1, area.y, area.width - left.width - 1, 1);
    if matches!(row, Row::Code { .. } | Row::NoNewline { .. }) {
        frame.render_widget(
            Span::styled("│", theme::dim()),
            Rect::new(left.right(), area.y, 1, 1),
        );
    }
    match row {
        Row::Hunk(header) | Row::Message(header) => {
            let style = if matches!(row, Row::Hunk(_)) {
                theme::blue()
            } else {
                theme::dim()
            };
            frame.render_widget(
                Span::styled(
                    truncate(&visible(header), usize::from(area.width - 2)),
                    style,
                ),
                Rect::new(area.x + 1, area.y, area.width - 2, 1),
            );
        }
        Row::Code { old, new } => {
            draw_side(frame, left, *old, false, gutter, horizontal, map);
            draw_side(frame, right, *new, true, gutter, horizontal, map);
        }
        Row::NoNewline { old, new } => {
            for (rect, show) in [(left, old), (right, new)] {
                if *show {
                    frame.render_widget(
                        Span::styled(
                            truncate(" ↳ No newline at EOF", usize::from(rect.width)),
                            theme::warn(),
                        ),
                        rect,
                    );
                }
            }
        }
    }
}

fn draw_side(
    frame: &mut Frame<'_>,
    area: Rect,
    line: Option<&DiffLine>,
    after: bool,
    gutter: u16,
    horizontal: usize,
    map: &mut FrameMap,
) {
    let Some(line) = line else {
        return;
    };
    let (number, mark, style) = match (after, line.kind) {
        (false, DiffLineKind::Deletion) => (line.old_number, "−", theme::error()),
        (true, DiffLineKind::Addition) => (line.new_number, "+", theme::ok()),
        (true, _) => (line.new_number, " ", theme::text()),
        (false, _) => (line.old_number, " ", theme::text()),
    };
    let changed = line.kind != DiffLineKind::Context;
    let number = number.map_or_else(String::new, |n| n.to_string());
    let digits = usize::from(gutter.saturating_sub(3));
    let label = format!("{number:>digits$} {mark} ");
    frame.render_widget(
        Span::styled(
            truncate(&label, usize::from(gutter)),
            if changed {
                style.add_modifier(Modifier::REVERSED)
            } else {
                theme::dim()
            },
        ),
        Rect::new(area.x, area.y, gutter, 1),
    );
    let text_area = Rect::new(
        area.x + gutter,
        area.y,
        area.width.saturating_sub(gutter),
        1,
    );
    let shown = window(
        &visible(&line.text),
        horizontal,
        usize::from(text_area.width),
    );
    frame.render_widget(Span::styled(shown.clone(), style), text_area);
    map.text.push(crate::hit::TextRow {
        at: Position::new(text_area.x, text_area.y),
        text: shown,
        continues: false,
    });
}

/// Control bytes in repository content must never reach the terminal backend as commands.
fn visible(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => shown.push_str("    "),
            c if c.is_control() => shown.extend(c.escape_default()),
            c => shown.push(c),
        }
    }
    shown
}

/// Pan in terminal cells rather than UTF-8 bytes, preserving a wide glyph's alignment.
fn window(text: &str, start: usize, room: usize) -> String {
    let mut at = 0;
    let mut shown = String::new();
    for c in text.chars() {
        let end = at + UnicodeWidthChar::width(c).unwrap_or(0);
        if at >= start {
            shown.push(c);
        } else if end > start {
            shown.push(' ');
        }
        at = end;
    }
    truncate(&shown, room)
}
