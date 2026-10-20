//! The slash-command menu: a box on the bottom of the conversation, just above the input box,
//! one row per matching command (`/name` and what it does), the highlighted row filled.

use crate::app::App;
use crate::hit::{FrameMap, Hit};
use crate::input::slash::{COMMANDS, SlashMenu};
use crate::text::{cells, truncate, width};
use crate::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

/// Cells between the longest name and the descriptions.
const GAP: usize = 3;

/// Draws `menu` into the bottom of `over` (the conversation), right above `input`, and records a
/// `SlashItem` per row.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    over: Rect,
    input: Rect,
    menu: &SlashMenu,
    app: &App,
    map: &mut FrameMap,
) {
    let wanted = cells(menu.entries.len() + 2);
    let height = wanted.min(over.height);
    if height < 3 || input.width < 8 {
        return;
    }
    let area = Rect::new(input.x, input.y.saturating_sub(height), input.width, height);
    frame.render_widget(Clear, area);
    // Rows run border to border, so the highlight fills the whole row.
    let block = crate::bridge::pane_block("Commands", theme::accent()).padding(Padding::ZERO);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Dialog);

    let room = usize::from(inner.width);
    // Over every command, so the descriptions don't move as the list narrows.
    let names = COMMANDS
        .iter()
        .map(|e| width(e.name) + 1)
        .max()
        .unwrap_or(0);
    let lines: Vec<Line<'static>> = menu
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let selected = i == menu.cursor;
            let hovered = app.term.hover == Some(Hit::SlashItem(i));
            let paint = |style: Style| {
                if selected {
                    theme::on_selection(style)
                } else {
                    style
                }
            };
            let name = format!(" /{}", entry.name);
            let lead = format!("{name:<w$}", w = names + 1 + GAP);
            let lead = truncate(&lead, room);
            let rest = room.saturating_sub(width(&lead));
            let description = truncate(entry.description, rest.saturating_sub(1));
            let pad = rest.saturating_sub(width(&description));
            // The name in the default colour (bold under the pointer), bright white on the fill
            // when highlighted, like every selected row.
            let name_style = if hovered || selected {
                paint(theme::strong())
            } else {
                theme::text()
            };
            Line::from(vec![
                Span::styled(lead, name_style),
                Span::styled(description, paint(theme::dim())),
                Span::styled(" ".repeat(pad), paint(theme::text())),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    for i in 0..menu.entries.len() {
        let Ok(row) = u16::try_from(i) else { break };
        if row >= inner.height {
            break;
        }
        map.hits.add(
            Rect::new(inner.x, inner.y + row, inner.width, 1),
            Hit::SlashItem(i),
        );
    }
}
