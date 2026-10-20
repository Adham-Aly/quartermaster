//! The app's own text selection.
//!
//! Capturing the mouse switches off the terminal's own selection, so the app draws one. Widgets
//! record each drawn row of selectable text as a [`TextRow`]; a drag selects screen cells from
//! the anchor to the head in reading order, restricted to those rows and to the pane the drag
//! started in.

use crate::hit::{FrameMap, TextRow};
use crate::view::Selection;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthChar;

/// The selection's two ends in reading order (top to bottom, then left to right).
pub(crate) fn ordered(selection: Selection) -> (Position, Position) {
    let (a, b) = (selection.anchor, selection.head);
    if (a.y, a.x) <= (b.y, b.x) {
        (a, b)
    } else {
        (b, a)
    }
}

/// The text rows a selection starting at `anchor` can cover: those in the scrollable pane under
/// the anchor (so a drag in the conversation never takes in the task pane beside it), or every
/// row when the anchor is in none.
pub(crate) fn rows_for(map: &FrameMap, anchor: Position) -> Vec<TextRow> {
    let area = map
        .scroll_areas
        .iter()
        .rev()
        .find(|(area, _)| area.contains(anchor))
        .map(|&(area, _)| area);
    map.text
        .iter()
        .filter(|row| area.is_none_or(|area| area.contains(row.at)))
        .cloned()
        .collect()
}

/// Whether `at` lies on a cell of one of `rows`.
pub(crate) fn on_text(rows: &[TextRow], at: Position) -> bool {
    rows.iter().any(|row| {
        let end = u32::from(row.at.x) + width_u32(&row.text);
        row.at.y == at.y && at.x >= row.at.x && u32::from(at.x) < end
    })
}

fn width_u32(s: &str) -> u32 {
    u32::try_from(crate::text::width(s)).unwrap_or(u32::MAX)
}

/// The inclusive range of screen columns selected on screen row `y`.
fn columns_on(y: u16, start: Position, end: Position) -> Option<(u16, u16)> {
    if y < start.y || y > end.y {
        return None;
    }
    let from = if y == start.y { start.x } else { 0 };
    let to = if y == end.y { end.x } else { u16::MAX };
    Some((from, to))
}

/// The part of `row` whose cells meet the inclusive column range, and whether any did.
fn cut(row: &TextRow, from: u16, to: u16) -> Option<String> {
    let mut x = u32::from(row.at.x);
    let mut out = String::new();
    let mut any = false;
    for c in row.text.chars() {
        let w = u32::try_from(c.width().unwrap_or(0)).unwrap_or(0);
        let last = x + w.max(1) - 1;
        if last >= u32::from(from) && x <= u32::from(to) {
            out.push(c);
            any = true;
        }
        x += w;
    }
    any.then_some(out)
}

/// The selected text: the rows between the two ends, each cut to the selected cells, trailing
/// spaces trimmed, joined with `\n`, or with one space before a row that continues a soft wrap.
pub(crate) fn text(rows: &[TextRow], selection: Selection) -> String {
    let (start, end) = ordered(selection);
    let mut out = String::new();
    let mut first = true;
    for row in rows {
        let Some((from, to)) = columns_on(row.at.y, start, end) else {
            continue;
        };
        let Some(part) = cut(row, from, to) else {
            continue;
        };
        if !first {
            out.push(if row.continues { ' ' } else { '\n' });
        }
        first = false;
        out.push_str(part.trim_end_matches(' '));
    }
    out
}

/// Reverses every selected cell that a text row covers.
pub(crate) fn paint(buffer: &mut Buffer, rows: &[TextRow], selection: Selection) {
    let (start, end) = ordered(selection);
    let area = buffer.area;
    for row in rows {
        let Some((from, to)) = columns_on(row.at.y, start, end) else {
            continue;
        };
        let row_end = u32::from(row.at.x) + width_u32(&row.text);
        let first = row.at.x.max(from);
        let last = u32::from(to).min(row_end.saturating_sub(1));
        let mut x = first;
        while u32::from(x) <= last {
            let position = Position::new(x, row.at.y);
            if area.contains(position)
                && let Some(cell) = buffer.cell_mut(position)
            {
                cell.modifier.insert(Modifier::REVERSED);
            }
            let Some(next) = x.checked_add(1) else { break };
            x = next;
        }
    }
}
