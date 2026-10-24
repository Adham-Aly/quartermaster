//! The Bridge's body: projects and crew on the left, the Quartermaster's conversation and your
//! input in the middle (or a worker's session in their place, `session`), the selected task on
//! the right.
//!
//! The columns default to 31 cells, and 36 for the task pane's 32 cells of
//! text, and can be resized by dragging the borders between them. Below 120 columns the task pane
//! folds away; clicking a task then opens it as an overlay anchored to the right edge.

use crate::app::{App, LEFT_RANGE, RIGHT_RANGE};
use crate::hit::{Divider, FrameMap, Hit, Pane};
use crate::router::Screen;
use crate::text::{cells, wrap};
use crate::theme;
use crate::widgets::{
    context_strip, conversation, crew, input_box, projects, selection, slash_menu, task,
};
use qm_types::api::AgentRef;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

/// Below this width the task pane folds away.
pub(crate) const FOLD_BELOW: u16 = 120;

/// The conversation column never gets narrower than this when the others are wide.
const MIDDLE_MIN: u16 = 36;

/// The folded overlay always leaves this many cells of the middle column in view.
const OVERLAY_LEAVES: u16 = 10;

/// Where the Bridge's parts go in a body area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Columns {
    left: Rect,
    middle: Rect,
    /// `None` when folded.
    right: Option<Rect>,
}

fn columns(body: Rect, app: &App) -> Columns {
    let folded = body.width < FOLD_BELOW;
    let gaps = if folded { 1 } else { 2 };
    let room = body.width.saturating_sub(gaps + MIDDLE_MIN);
    let mut left = app.left_width.clamp(LEFT_RANGE.0, LEFT_RANGE.1);
    let mut right = if folded {
        0
    } else {
        app.right_width.clamp(RIGHT_RANGE.0, RIGHT_RANGE.1)
    };
    // Too wide for both: the task pane gives way first, then the left column.
    if left + right > room {
        right = right
            .min(room.saturating_sub(left))
            .max(if folded { 0 } else { RIGHT_RANGE.0 });
        left = left.min(room.saturating_sub(right)).max(LEFT_RANGE.0);
    }
    let left_rect = Rect::new(body.x, body.y, left, body.height);
    let middle_x = left_rect.right() + 1;
    let right_rect = (!folded).then(|| {
        Rect::new(
            body.right().saturating_sub(right),
            body.y,
            right,
            body.height,
        )
    });
    let middle_end = right_rect.map_or(body.right(), |r| r.x.saturating_sub(1));
    Columns {
        left: left_rect,
        middle: Rect::new(
            middle_x,
            body.y,
            middle_end.saturating_sub(middle_x),
            body.height,
        ),
        right: right_rect,
    }
}

/// Draws the Bridge body and records its clickable regions.
pub(crate) fn draw_body(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    let cols = columns(body, app);
    map.folded = cols.right.is_none();

    // Left column: projects and crew as tall as their rows (Projects within a quarter, plus any
    // rows Actions can spare), then the Actions inbox to the bottom.
    let inner_width = cols.left.width.saturating_sub(4);
    let (projects_height, crew_height, actions_height) = crate::widgets::sidebar::heights(
        cols.left.height,
        cells(projects::rows(app, inner_width).len()),
        cells(crew::rows(app, inner_width).len()),
        cells(app.snapshot.actions.len()),
    );
    let projects_area = Rect {
        height: projects_height,
        ..cols.left
    };
    let crew_area = Rect {
        y: projects_area.bottom(),
        height: crew_height,
        ..cols.left
    };
    let actions_area = Rect {
        y: crew_area.bottom(),
        height: actions_height,
        ..cols.left
    };
    projects::draw(frame, projects_area, app, map);
    crew::draw(frame, crew_area, app, map);
    crate::widgets::actions::draw(frame, actions_area, app, map);

    // Middle column: a worker's session, when one is open, or the conversation above your input.
    if let Some(agent) = app.shown_session() {
        crate::session::draw(frame, cols.middle, app, agent, map);
    } else {
        draw_conversation(frame, cols.middle, app, map);
    }

    // Right column: the selected task.
    if let Some(task_area) = cols.right {
        task::draw(frame, task_area, app, map, false);
    }

    // The dividers, added after the panes so a press on a border starts a drag rather than a
    // focus.
    let mut dividers = vec![(Divider::Left, cols.left.right())];
    if let Some(task_area) = cols.right {
        dividers.push((Divider::Right, task_area.x - 1));
    }
    for (divider, gap) in dividers {
        let grip = Rect::new(gap.saturating_sub(1), body.y, 3, body.height);
        map.hits.add(grip, Hit::Divider(divider));
        if app.term.drag == Some(divider) || app.term.hover == Some(Hit::Divider(divider)) {
            draw_grip(frame, Rect::new(gap, body.y, 1, body.height));
        }
    }

    // Folded: the task pane as an overlay on the right, drawn and recorded last.
    if map.folded && app.view.task_overlay {
        let widest = body.right().saturating_sub(cols.middle.x + OVERLAY_LEAVES);
        let width = app.right_width.min(widest).min(body.width);
        let overlay = Rect::new(body.right() - width, body.y, width, body.height);
        frame.render_widget(Clear, overlay);
        task::draw(frame, overlay, app, map, true);
    }
}

/// The middle column's Quartermaster: the conversation above your input, and the slash-command
/// menu while it shows.
fn draw_conversation(frame: &mut Frame<'_>, middle: Rect, app: &App, map: &mut FrameMap) {
    if let Some(notice) = app
        .snapshot
        .model_setup_notice
        .as_ref()
        .filter(|_| app.model_gated())
    {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::dim())
            .title(" Quartermaster ");
        let inside = block.inner(middle);
        frame.render_widget(block, middle);
        let lines = wrap(notice, inside.width.saturating_sub(2) as usize);
        let height = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .min(inside.height);
        let area = Rect {
            y: inside.y + inside.height.saturating_sub(height) / 2,
            height,
            ..inside
        };
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .style(theme::dim())
                .alignment(ratatui::layout::Alignment::Center),
            area,
        );
        return;
    }
    let agent = AgentRef::Quartermaster;
    let input_height = input_box::height(app, middle.width);
    let [conversation_area, strip_area, input_area] =
        context_strip::split(app, middle, input_height, agent);
    map.conversation_scroll_max = conversation::draw(frame, conversation_area, app, map);
    context_strip::draw(frame, strip_area, app, agent);
    input_box::draw(frame, input_area, app, map);
    // The slash-command menu sits on the conversation, just above the strip and the input box.
    if let Some(menu) = app.slash_menu() {
        let anchor = Rect {
            y: strip_area.y,
            height: strip_area.height + input_area.height,
            ..input_area
        };
        slash_menu::draw(frame, conversation_area, anchor, &menu, app, map);
    }
}

/// The highlighted gap between two columns while a drag is possible or under way.
fn draw_grip(frame: &mut Frame<'_>, area: Rect) {
    let lines: Vec<Line<'_>> = (0..area.height)
        .map(|_| Line::styled("│", theme::accent()))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// A pane's rounded frame, titled ("╭─ Projects ─…").
pub(crate) fn pane_block(title: &str, style: Style) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style)
        .title(Line::from(vec![
            Span::styled("─", style),
            Span::styled(format!(" {title} "), style),
        ]))
        .padding(Padding::horizontal(1))
}

/// A pane's border style: accent when focused, the text colour when hovered, dim otherwise.
pub(crate) fn pane_style(app: &App, pane: Pane) -> Style {
    let focused = app.screen == Screen::Bridge && app.focus == pane;
    theme::border(focused, app.term.hover == Some(Hit::Pane(pane)))
}

/// Lines of dim text for an empty pane, wrapped to its width.
pub(crate) fn empty_lines(texts: &[&str], max: u16) -> Vec<Line<'static>> {
    texts
        .iter()
        .flat_map(|t| wrap(t, usize::from(max)))
        .map(|l| Line::styled(l, theme::dim()))
        .collect()
}

/// One row of a list pane (projects, crew).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListRow {
    pub(crate) line: Line<'static>,
    /// What a click on the row does; recorded across the box's inner width.
    pub(crate) hit: Option<Hit>,
    /// The selected row: `Some(true)` filled, padding included; `Some(false)` only marked, after
    /// a mouse press (`widgets::selection`).
    pub(crate) selected: Option<bool>,
}

impl ListRow {
    /// A row that can't be clicked or selected.
    pub(crate) fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            hit: None,
            selected: None,
        }
    }
}

/// Draws a list pane: its box, the pane's hit, then the visible rows from `scroll` with their
/// hits, and records the pane's scroll area and maximum.
pub(crate) fn draw_list(
    frame: &mut Frame<'_>,
    area: Rect,
    block: Block<'static>,
    rows: Vec<ListRow>,
    scroll: usize,
    pane: Pane,
    map: &mut FrameMap,
) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Pane(pane));
    map.scroll_areas.push((area, pane));
    let max = rows.len().saturating_sub(usize::from(inner.height));
    map.scroll_max.push((pane, max));
    // The row's full width inside the border, padding included.
    let wide_x = area.x.saturating_add(1);
    let wide_width = area.width.saturating_sub(2);
    for (y, row) in (inner.y..inner.bottom()).zip(rows.into_iter().skip(scroll.min(max))) {
        let wide = Rect::new(wide_x, y, wide_width, 1);
        frame.render_widget(row.line, Rect::new(inner.x, y, inner.width, 1));
        if let Some(filled) = row.selected {
            selection::paint(frame.buffer_mut(), wide, area, filled);
        }
        if let Some(hit) = row.hit {
            map.hits.add(wide, hit);
        }
    }
}

#[cfg(test)]
mod model_setup_notice_tests {
    use super::*;
    use crate::input::draft::Draft;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use qm_types::api::{AgentRef, Command, Update};
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Instant;

    const NOTICE: &str =
        "Choose a model in Settings before sending a message to the Quartermaster.";

    fn blocked(text: &str) -> App {
        let mut app = App::new();
        app.snapshot.model_setup_notice = Some(NOTICE.to_owned());
        app.focus = Pane::Input;
        app.draft = Draft::new(text.to_owned());
        app.take_commands();
        app
    }

    #[test]
    fn model_setup_notice_is_centered_dim_and_wraps_at_narrow_widths() {
        for width in [80, 36, 20] {
            let app = blocked("unsent draft");
            let area = Rect::new(0, 0, width, 18);
            let mut terminal = Terminal::new(TestBackend::new(width, area.height)).unwrap();
            let mut map = FrameMap::default();
            terminal
                .draw(|frame| draw_conversation(frame, area, &app, &mut map))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let lines = wrap(NOTICE, usize::from(width - 4));
            let first_y = 1 + (16 - u16::try_from(lines.len()).unwrap()) / 2;
            for (offset, line) in lines.iter().enumerate() {
                let y = first_y + u16::try_from(offset).unwrap();
                let row: String = (1..width - 1).map(|x| buffer[(x, y)].symbol()).collect();
                assert_eq!(row.trim(), line);
                let x = 1 + (width - 2 - u16::try_from(line.len()).unwrap()).div_ceil(2);
                assert_eq!(row.find(line).unwrap(), usize::from(x - 1));
                for column in x..x + u16::try_from(line.len()).unwrap() {
                    assert_eq!(buffer[(column, y)].fg, theme::dim().fg.unwrap());
                }
            }
            assert!(map.hits.rect_of(Hit::Pane(Pane::Input)).is_none());
            assert_eq!(map.scroll_areas, [] as [(Rect, Pane); 0]);
        }
    }

    #[test]
    fn model_setup_notice_blocks_enter_ctrl_shortcuts_and_slash_dispatch() {
        for text in ["keep this draft", "/clear", "/compact"] {
            for (code, modifiers) in [
                (KeyCode::Enter, KeyModifiers::NONE),
                (KeyCode::Enter, KeyModifiers::CONTROL),
                (KeyCode::Char('j'), KeyModifiers::CONTROL),
                (KeyCode::Char('d'), KeyModifiers::CONTROL),
                (KeyCode::Char('/'), KeyModifiers::NONE),
            ] {
                let mut app = blocked(text);
                app.on_key(KeyEvent::new(code, modifiers), Instant::now());
                assert!(app.take_commands().is_empty(), "{text}: {code:?}");
                assert_eq!(app.draft.text, text, "{code:?}");
                app.send_input();
                assert_eq!(app.take_commands(), [] as [Command; 0]);
                assert_eq!(app.draft.text, text);
            }
        }
    }

    #[test]
    fn model_setup_notice_comma_opens_settings_with_input_focused() {
        let mut app = blocked("keep this draft");
        app.on_key(
            KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE),
            Instant::now(),
        );
        assert_eq!(app.popup(), Some(crate::popups::Popup::Settings));
        assert_eq!(app.draft.text, "keep this draft");
        assert_eq!(app.focus, Pane::Input);
    }

    #[test]
    fn model_setup_notice_unblocking_snapshot_preserves_draft_and_allows_send() {
        let mut app = blocked("keep this draft");
        app.on_update(Update::Snapshot(Box::default()));
        assert!(app.snapshot.model_setup_notice.is_none());
        assert_eq!(app.draft.text, "keep this draft");
        app.on_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Instant::now(),
        );
        assert_eq!(
            app.take_commands(),
            [Command::Send {
                to: AgentRef::Quartermaster,
                text: "keep this draft".to_owned(),
                images: Vec::new(),
            }]
        );
        assert_eq!(app.draft.text, "");
    }
}
