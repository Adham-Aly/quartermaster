//! The crew tree: the Quartermaster at the root, its crewmates and secondmates below, each with a
//! status glyph and an aligned status column.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width};
use crate::theme;
use qm_types::api::{AgentRef, CrewRow, Waiting};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Draws the crew box into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let block = pane_block("Crew", pane_style(app, Pane::Crew));
    let rows = rows(app, block.inner(area).width);
    draw_list(
        frame,
        area,
        block,
        rows,
        app.view.crew_scroll,
        Pane::Crew,
        map,
    );
}

/// The pane's rows at `inner_width`; their count sizes the box.
pub(crate) fn rows(app: &App, inner_width: u16) -> Vec<ListRow> {
    let crew = &app.snapshot.crew;
    if crew.is_empty() {
        // The Quartermaster is always aboard, even before the core reports on it.
        let mut out = vec![ListRow::plain(Line::from(vec![
            Span::styled("◆", theme::accent()),
            Span::raw(" Quartermaster"),
        ]))];
        out.extend(
            empty_lines(&["No crewmates yet."], inner_width)
                .into_iter()
                .map(ListRow::plain),
        );
        return out;
    }
    let columns = fit_columns(crew, status_columns(crew), usize::from(inner_width));
    let mut out: Vec<ListRow> = crew
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let hit = Hit::Crew(row.agent);
            let selected = !app.view.pointer_picked && app.view.selected_crew == Some(row.agent);
            let hovered = !selected && app.hover == Some(hit);
            ListRow {
                line: line(crew, i, columns[i], hovered),
                hit: Some(hit),
                selected,
            }
        })
        .collect();
    // The Quartermaster alone: still no crewmates, whatever its status says.
    if crew.iter().all(|r| r.agent == AgentRef::Quartermaster) {
        out.extend(
            empty_lines(&["No crewmates yet."], inner_width)
                .into_iter()
                .map(ListRow::plain),
        );
    }
    out
}

/// Caps the status columns so every status fits inside `inner_width`: when a label is too long
/// for its status to follow it, the status moves left (labels are cut to make room) rather than
/// off the pane.
fn fit_columns(rows: &[CrewRow], columns: Vec<usize>, inner_width: usize) -> Vec<usize> {
    let widest = rows.iter().map(|r| width(&r.status)).max().unwrap_or(0);
    let cap = inner_width.saturating_sub(widest);
    columns
        .into_iter()
        .zip(rows)
        .map(|(column, row)| column.min(cap).max(prefix_width(row) + 2))
        .collect()
}

/// The cells before a row's label: `◆ ` at the root, `├ ● ` at depth 1, two more per level.
fn prefix_width(row: &CrewRow) -> usize {
    match row.depth {
        0 => 2,
        d => 2 * usize::from(d) + 2,
    }
}

/// The index of each row's parent: the nearest earlier row one level up.
fn parent_of(rows: &[CrewRow], i: usize) -> Option<usize> {
    let depth = rows[i].depth;
    if depth == 0 {
        return None;
    }
    rows[..i].iter().rposition(|r| r.depth == depth - 1)
}

/// The cell each row's status starts at: among siblings (same depth, same parent), 2 cells after
/// the longest prefix-plus-label; a root's status one cell left of its reports' column, or 2 after
/// its label when it has none.
pub(crate) fn status_columns(rows: &[CrewRow]) -> Vec<usize> {
    let parents: Vec<Option<usize>> = (0..rows.len()).map(|i| parent_of(rows, i)).collect();
    let sibling_column = |i: usize| {
        (0..rows.len())
            .filter(|&j| rows[j].depth == rows[i].depth && parents[j] == parents[i])
            .map(|j| prefix_width(&rows[j]) + width(&rows[j].label))
            .max()
            .unwrap_or(0)
            + 2
    };
    (0..rows.len())
        .map(|i| {
            let row = &rows[i];
            let end = prefix_width(row) + width(&row.label);
            if row.depth > 0 {
                return sibling_column(i);
            }
            match (0..rows.len()).find(|&j| parents[j] == Some(i)) {
                Some(report) => (sibling_column(report) - 1).max(end + 1),
                None => end + 2,
            }
        })
        .collect()
}

/// Whether no later sibling follows row `i` (it draws `└`, not `├`).
fn is_last(rows: &[CrewRow], i: usize) -> bool {
    let depth = rows[i].depth;
    rows[i + 1..]
        .iter()
        .take_while(|r| r.depth >= depth)
        .all(|r| r.depth > depth)
}

/// What a row is waiting on you for, if anything (`needs_you` alone reads as an approval).
fn waiting(row: &CrewRow) -> Option<Waiting> {
    row.waiting
        .or_else(|| row.needs_you.then_some(Waiting::Approval))
}

/// Statuses of agents doing nothing right now, drawn with a dim `○`: an idle secondmate, and a
/// task that is queued, waits, has reported or has landed. A flagged row
/// keeps its `⚑`; every other row is working and gets the green `●`.
const RESTING: &[&str] = &["idle", "queued", "waits", "reported", "landed"];

fn waiting_style(waiting: Waiting) -> Style {
    match waiting {
        Waiting::Decision => theme::warn(),
        _ => theme::accent(),
    }
}

/// One row of the tree, its status starting at cell `column`.
fn line(rows: &[CrewRow], i: usize, column: usize, hovered: bool) -> Line<'static> {
    let row = &rows[i];
    let mut spans = Vec::new();
    if row.depth == 0 {
        spans.push(Span::styled("◆", theme::accent()));
        spans.push(Span::raw(" "));
    } else {
        let indent = "  ".repeat(usize::from(row.depth - 1));
        let branch = if is_last(rows, i) { "└" } else { "├" };
        spans.push(Span::raw(indent));
        spans.push(Span::styled(branch, theme::dim()));
        spans.push(Span::raw(" "));
        let (glyph, style) = match waiting(row) {
            Some(w) => ("⚑", waiting_style(w)),
            None if RESTING.contains(&row.status.as_str()) => ("○", theme::dim()),
            None => ("●", theme::ok()),
        };
        spans.push(Span::styled(glyph, style));
        spans.push(Span::raw(" "));
    }
    let label_style = if hovered {
        theme::text().add_modifier(Modifier::UNDERLINED)
    } else {
        theme::text()
    };
    // A label too long for its status column is cut, keeping a space before the status.
    let room = column.saturating_sub(prefix_width(row) + 1);
    let label = if width(&row.label) > room {
        truncate(&row.label, room)
    } else {
        row.label.clone()
    };
    let used = prefix_width(row) + width(&label);
    spans.push(Span::styled(label, label_style));
    spans.push(Span::raw(" ".repeat(column.saturating_sub(used).max(1))));
    let status_style = match waiting(row) {
        Some(w) => waiting_style(w),
        None if row.agent == AgentRef::Quartermaster && row.status == "ready" => theme::ok(),
        None => theme::dim(),
    };
    spans.push(Span::styled(row.status.clone(), status_style));
    Line::from(spans)
}
