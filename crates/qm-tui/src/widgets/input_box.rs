//! The input box: `› ` and your message, the placeholder when empty, and the `▌` cursor glyph
//! wherever you are editing your text.

use crate::app::{App, Quit};
use crate::hit::{FrameMap, Hit, Pane};
use crate::router::Screen;
use crate::text::{cells, tail, truncate, width};
use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};

/// The box grows with your newlines up to this many text rows, then scrolls.
pub(crate) const MAX_LINES: usize = 5;

/// The box always shows at least this many text rows.
const MIN_LINES: usize = 2;

/// The placeholder in the empty box.
const PLACEHOLDER: &str = "message the Quartermaster…";

/// The cursor glyph.
const CURSOR: &str = "▌";

/// The box's height for `input`: its rows (2 to 5) and the border.
pub(crate) fn height(input: &str) -> u16 {
    cells(input.split('\n').count().clamp(MIN_LINES, MAX_LINES) + 2)
}

/// Draws the input box into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let focused = app.screen == Screen::Bridge && app.focus == Pane::Input;
    let holds_text = !app.input.is_empty();
    let style = if focused || holds_text {
        theme::accent()
    } else {
        theme::border(false, app.hover == Some(Hit::Pane(Pane::Input)))
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style)
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Pane(Pane::Input));
    if inner.height == 0 || inner.width < 4 {
        return;
    }
    let room = usize::from(inner.width - 2);
    // The `▌` glyph marks where you are editing your text; an empty box shows only the
    // placeholder (the terminal's own cursor sits there).
    let (lines, cursor) = if holds_text {
        text_lines(app, room, usize::from(inner.height))
    } else {
        let spans = vec![
            Span::styled("› ", theme::accent()),
            Span::styled(truncate(&placeholder(app), room), theme::dim()),
        ];
        (vec![Line::from(spans)], (0, 0))
    };
    frame.render_widget(Paragraph::new(lines), inner);
    // The terminal's own cursor follows, for input methods, unless a dialog has the keyboard.
    if focused && !matches!(app.quit, Quit::Confirm { .. } | Quit::Requested) {
        let (row, column) = cursor;
        frame.set_cursor_position(Position::new(
            inner.x + 2 + cells(column),
            inner.y + cells(row),
        ));
    }
}

/// The empty box's placeholder: in guidance mode it names the card you are guiding.
fn placeholder(app: &App) -> String {
    if app.view.location_for.is_some() {
        return String::from("the folder for this project, such as ~/Developer…");
    }
    let Some(held) = app.view.guidance_for else {
        return PLACEHOLDER.to_owned();
    };
    let card = app
        .snapshot
        .conversation
        .iter()
        .filter_map(|chat| chat.card.as_ref())
        .find(|card| card.held == Some(held));
    match card {
        Some(card) => format!(
            "your guidance for {}…",
            card.title.as_deref().unwrap_or(&card.heading)
        ),
        None => String::from("your guidance…"),
    }
}

/// Your message's rows, scrolled so the cursor shows, with the cursor glyph in place; and the
/// cursor's (row, column) in the drawn rows, columns counted after the `› ` lead.
fn text_lines(app: &App, room: usize, visible: usize) -> (Vec<Line<'static>>, (usize, usize)) {
    let input = &app.input;
    let at = app
        .view
        .cursor
        .filter(|&c| c <= input.len() && input.is_char_boundary(c))
        .unwrap_or(input.len());
    let cursor_line = input[..at].matches('\n').count();
    let all: Vec<&str> = input.split('\n').collect();
    let first = (cursor_line + 1).saturating_sub(visible);
    let mut cursor = (0, 0);
    let lines = all
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, text)| {
            let lead = if i == 0 {
                Span::styled("› ", theme::accent())
            } else {
                Span::raw("  ")
            };
            if i != cursor_line {
                return Line::from(vec![lead, Span::raw(truncate(text, room))]);
            }
            let line_start = input[..at].rfind('\n').map_or(0, |n| n + 1);
            let (before, after) = text.split_at(at - line_start);
            // Keep the glyph in view: show the end of what comes before it.
            let before = tail(before, room.saturating_sub(1));
            let after_room = room.saturating_sub(width(before) + 1);
            cursor = (i - first, width(before));
            Line::from(vec![
                lead,
                Span::raw(before.to_owned()),
                Span::styled(CURSOR, theme::accent()),
                Span::raw(cut(after, after_room)),
            ])
        })
        .collect();
    (lines, cursor)
}

/// `s` cut to `max` cells, with no ellipsis (the text runs on past the box's edge).
fn cut(s: &str, max: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|&c| {
            used += width(c.encode_utf8(&mut [0; 4]));
            used <= max
        })
        .collect()
}
