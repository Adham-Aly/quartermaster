//! A dropdown list: a bordered box of one-line items over everything beneath it, the item under
//! the keyboard cursor reversed. Context menus and the top bar's menu both draw through it.
//!
//! Each item can carry a key hint (dim, on the right) and a badge: a red dot after its label that
//! says something there needs you.

use crate::hit::{Hit, Hits};
use crate::text;
use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

/// The dot a badge draws.
pub(crate) const BADGE: &str = "●";

/// Cells between an item's label (and badge) and its key hint.
const HINT_GAP: usize = 3;

/// One row of a dropdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Item<'a> {
    pub(crate) label: &'a str,
    /// The key that does the same, shown dim on the right; empty for none.
    pub(crate) hint: &'a str,
    /// A red dot after the label.
    pub(crate) badge: bool,
}

impl<'a> Item<'a> {
    pub(crate) const fn new(label: &'a str) -> Self {
        Self {
            label,
            hint: "",
            badge: false,
        }
    }

    pub(crate) const fn hint(self, hint: &'a str) -> Self {
        Self { hint, ..self }
    }

    pub(crate) const fn badge(self, badge: bool) -> Self {
        Self { badge, ..self }
    }

    /// The label and its badge, in cells.
    fn head_width(&self) -> usize {
        text::width(self.label) + if self.badge { 2 } else { 0 }
    }
}

/// Where a dropdown of `items` opened at `at` sits: its first item on `at`'s row, starting at
/// `at`'s column, shifted to stay inside `screen`.
pub(crate) fn area(items: &[Item<'_>], at: Position, screen: Rect) -> Rect {
    let heads = items.iter().map(Item::head_width).max().unwrap_or(0);
    let hints = items
        .iter()
        .map(|item| text::width(item.hint))
        .max()
        .unwrap_or(0);
    let row = heads + if hints > 0 { HINT_GAP + hints } else { 0 };
    // A border and one cell of padding on each side.
    let width = text::cells(row + 4).min(screen.width);
    let height = text::cells(items.len() + 2).min(screen.height);
    let x = at.x.min(screen.right().saturating_sub(width)).max(screen.x);
    let y =
        at.y.saturating_sub(1)
            .min(screen.bottom().saturating_sub(height))
            .max(screen.y);
    Rect::new(x, y, width, height)
}

/// Draws `items` into `area` (from [`area`]) over everything beneath, item `cursor` reversed, and
/// records the regions: `Hit::Dialog` over the whole box, then `hit(i)` per item row.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    items: &[Item<'_>],
    cursor: usize,
    hits: &mut Hits,
    hit: impl Fn(usize) -> Hit,
) {
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    hits.add(area, Hit::Dialog);
    let lines: Vec<Line<'static>> = items
        .iter()
        .enumerate()
        .map(|(i, item)| row(item, usize::from(inner.width), i == cursor))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    for i in 0..items.len() {
        let Ok(row) = u16::try_from(i) else { break };
        if row >= inner.height {
            break;
        }
        hits.add(Rect::new(inner.x, inner.y + row, inner.width, 1), hit(i));
    }
}

/// One item's row, `room` cells wide: ` label ●   hint `, reversed under the cursor. The hint is
/// left out when the row is too narrow for it.
fn row(item: &Item<'_>, room: usize, current: bool) -> Line<'static> {
    let style = |base: ratatui::style::Style| {
        if current {
            base.add_modifier(Modifier::REVERSED)
        } else {
            base
        }
    };
    let badge = if item.badge { 2 } else { 0 };
    let hint = if item.hint.is_empty()
        || 1 + item.head_width() + HINT_GAP + text::width(item.hint) + 1 > room
    {
        ""
    } else {
        item.hint
    };
    let hint_room = if hint.is_empty() {
        0
    } else {
        text::width(hint) + 1
    };
    let label = text::truncate(
        &format!(" {}", item.label),
        room.saturating_sub(badge + hint_room),
    );
    let mut spans = vec![Span::styled(label.clone(), style(theme::text()))];
    let mut used = text::width(&label);
    if item.badge && used + 2 <= room {
        spans.push(Span::styled(" ", style(theme::text())));
        spans.push(Span::styled(BADGE, style(theme::badge())));
        used += 2;
    }
    let pad = room.saturating_sub(used + hint_room);
    spans.push(Span::styled(" ".repeat(pad), style(theme::text())));
    if !hint.is_empty() {
        spans.push(Span::styled(format!("{hint} "), style(theme::dim())));
    }
    Line::from(spans)
}
