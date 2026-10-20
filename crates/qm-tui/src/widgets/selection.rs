//! The selected row of a list (projects, crew, Settings): what the keys act on.
//!
//! While you use the keyboard it is filled. After a mouse press the fill would stay behind as
//! shading the pointer left long ago, so the row is only marked with an accent `▸` in its left
//! padding (see `View::pointer_picked`).

use crate::theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;

/// The mark beside a selected row whose fill is hidden.
pub(crate) const MARK: &str = "▸";

/// Paints `row` (screen cells, already drawn, clipped to `clip`) as the selected row: `filled`,
/// or else marked. The mark goes in the row's first cell when it is blank (the padding), or in
/// the blank cell just before it.
pub(crate) fn paint(buf: &mut Buffer, row: Rect, clip: Rect, filled: bool) {
    let row = row.intersection(clip);
    if row.is_empty() {
        return;
    }
    if filled {
        fill(buf, row);
        return;
    }
    let blank = |buf: &Buffer, x: u16| {
        buf.cell(Position::new(x, row.y))
            .is_some_and(|cell| cell.symbol() == " ")
    };
    let at = if blank(buf, row.x) {
        Some(row.x)
    } else {
        row.x
            .checked_sub(1)
            .filter(|&x| x >= clip.x && blank(buf, x))
    };
    if let Some(x) = at {
        buf.set_string(x, row.y, MARK, theme::accent());
    }
}

/// The fill's background under every cell that has none of its own, with dim and default text
/// brightened so it reads on the fill. A button on the row would vanish into the fill (both
/// bright black), so it turns black on bright white.
fn fill(buf: &mut Buffer, row: Rect) {
    for x in row.x..row.right() {
        if let Some(cell) = buf.cell_mut(Position::new(x, row.y)) {
            if cell.bg == Color::Reset {
                let style = theme::on_selection(cell.style());
                cell.set_style(style);
            } else if cell.bg == theme::DIM {
                cell.set_style(theme::button_on_selection());
            }
        }
    }
}
