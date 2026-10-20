//! The prompt field, in every chat: the Bridge's message to the Quartermaster and a worker
//! session's direct message. `› ` and your message soft-wrapped, the field growing with it and
//! then scrolling, the middle of a very long message folded (`input::field`), your selection
//! reversed and the cursor's cell on the accent; the placeholder when empty.

use crate::app::{App, Quit};
use crate::hit::{FrameMap, Hit, Pane};
use crate::input::field::{self, Drawn, Shown};
use crate::router::Screen;
use crate::text::{cells, truncate};
use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};

/// The placeholder in the Bridge's empty box.
const PLACEHOLDER: &str = "message the Quartermaster…";

/// The lead before the first row.
const LEAD: &str = "› ";

/// How a field shows.
pub(crate) struct Field {
    /// Shown dim while the field is empty or takes no text.
    pub(crate) placeholder: String,
    /// The field takes text (a closed session's doesn't).
    pub(crate) open: bool,
    /// The field has the keyboard: the terminal's own cursor follows it.
    pub(crate) focused: bool,
    /// The border is the accent rather than dim.
    pub(crate) accent: bool,
}

/// The cells a row of text gets in a box `width` wide: less the borders, the padding and the
/// `› ` lead.
fn room(width: u16) -> usize {
    usize::from(width.saturating_sub(6)).max(1)
}

/// The box's height for your message in a box `width` wide: its shown rows (see
/// `input::field`) and the border.
pub(crate) fn height(app: &App, width: u16) -> u16 {
    let rows = field::wrap(&app.draft.text, room(width));
    let cursor = field::row_of(&rows, app.cursor());
    cells(field::visible_rows(field::fold(rows.len(), cursor).len()) + 2)
}

/// Draws the Bridge's input box into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let focused = app.screen == Screen::Bridge && app.focus == Pane::Input;
    let field = Field {
        placeholder: placeholder(app),
        open: true,
        focused,
        accent: focused || !app.draft.text.is_empty(),
    };
    draw_field(frame, area, app, &field, map);
}

/// Draws a prompt field into `area` and records it in the map.
pub(crate) fn draw_field(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    field: &Field,
    map: &mut FrameMap,
) {
    let style = if field.accent {
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
    let lead_style = if field.open {
        theme::accent()
    } else {
        theme::dim()
    };
    if !field.open || app.draft.text.is_empty() {
        // An empty box shows only the placeholder; the terminal's own cursor sits at its start.
        let line = Line::from(vec![
            Span::styled(LEAD, lead_style),
            Span::styled(truncate(&field.placeholder, room), theme::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
        if field.open {
            map.field = Some(Drawn {
                area: Rect::new(inner.x + 2, inner.y, inner.width - 2, inner.height),
                room,
                len: 0,
                rows: field::wrap("", room),
                shown: vec![Shown::Row(0)],
                scroll: 0,
            });
            set_cursor(frame, app, field, Position::new(inner.x + 2, inner.y));
        }
        return;
    }

    let input = &app.draft.text;
    let at = app.cursor();
    let selection = app.selection();
    let rows = field::wrap(input, room);
    let cursor_row = field::row_of(&rows, at);
    let shown = field::fold(rows.len(), cursor_row);
    let cursor_index = shown
        .iter()
        .position(|s| *s == Shown::Row(cursor_row))
        .unwrap_or(0);
    let visible = usize::from(inner.height);
    let scroll = field::scroll_to(app.view.input_scroll, cursor_index, shown.len(), visible);
    for (offset, item) in shown.iter().skip(scroll).take(visible).enumerate() {
        // Each row may use the right padding cell: the cursor after a full row sits there.
        let row_area = Rect::new(inner.x, inner.y + cells(offset), inner.width + 1, 1);
        let line = match *item {
            Shown::Row(i) => {
                let lead = if i == 0 {
                    Span::styled(LEAD, lead_style)
                } else {
                    Span::raw("  ")
                };
                let cursor = (i == cursor_row).then_some(at);
                let mut spans = vec![lead];
                spans.extend(row_spans(input, rows[i], cursor, selection));
                Line::from(spans)
            }
            Shown::Fold { hidden, .. } => Line::from(vec![
                Span::raw("  "),
                Span::styled(truncate(&field::fold_label(hidden), room), theme::dim()),
            ]),
        };
        frame.render_widget(Paragraph::new(line), row_area);
    }
    let column = field::column_of(input, &rows, at).min(room);
    let row = cursor_index.saturating_sub(scroll);
    map.field = Some(Drawn {
        area: Rect::new(inner.x + 2, inner.y, inner.width - 2, inner.height),
        room,
        len: input.len(),
        rows,
        shown,
        scroll,
    });
    if row < visible {
        set_cursor(
            frame,
            app,
            field,
            Position::new(inner.x + 2 + cells(column), inner.y + cells(row)),
        );
    }
}

/// The terminal's own cursor follows the field, for input methods, unless a dialog has the
/// keyboard.
fn set_cursor(frame: &mut Frame<'_>, app: &App, field: &Field, at: Position) {
    if field.focused && !matches!(app.quit, Quit::Confirm { .. } | Quit::Requested) {
        frame.set_cursor_position(at);
    }
}

/// Row `row` of `input` as runs of one style: selected text reversed, the cursor's cell (a blank
/// one at the row's end) on the accent.
fn row_spans(
    input: &str,
    row: field::Row,
    cursor: Option<usize>,
    selection: Option<(usize, usize)>,
) -> Vec<Span<'static>> {
    let style_at = |i: usize| {
        if cursor == Some(i) {
            theme::cursor()
        } else if selection.is_some_and(|(from, to)| from <= i && i < to) {
            theme::selected()
        } else {
            theme::text()
        }
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (j, c) in input[row.start..row.end].char_indices() {
        let style = style_at(row.start + j);
        if run_style != Some(style) {
            if let Some(done) = run_style {
                spans.push(Span::styled(std::mem::take(&mut run), done));
            }
            run_style = Some(style);
        }
        run.push(c);
    }
    if let Some(done) = run_style {
        spans.push(Span::styled(run, done));
    }
    if cursor == Some(row.end) {
        spans.push(Span::styled(" ", theme::cursor()));
    }
    spans
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
