//! The task pane: the selected crewmate's task, in place in the right column or as the folded
//! overlay.

use super::button;
use super::conversation::{Row, RowText, draw_rows};
use crate::app::App;
use crate::bridge::{empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane, TaskButton};
use crate::text::{cells, truncate, width, wrap_parts};
use crate::theme;
use qm_types::api::{AgentRef, TaskDetail};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Labels are padded to this many cells, then two spaces before the value.
const LABEL_WIDTH: usize = 7;
const VALUE_AT: usize = LABEL_WIDTH + 2;

/// The overlay's close mark in its title border.
const CLOSE: &str = " ✕ ";

/// Draws the task box into `area`; `overlay` adds the close mark (the folded layout).
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    map: &mut FrameMap,
    overlay: bool,
) {
    let title = match app.view.selected_crew {
        Some(AgentRef::Crewmate { task }) => task.to_string(),
        Some(AgentRef::Secondmate { id }) => id.to_string(),
        _ => String::from("Task"),
    };
    let style = pane_style(app, Pane::Task);
    let mut block = pane_block(&title, style);
    let close_hovered = app.term.hover == Some(Hit::CloseOverlay);
    if overlay {
        let close_style = if close_hovered {
            theme::accent()
        } else {
            style
        };
        block = block.title_top(
            Line::from(vec![
                Span::styled(CLOSE, close_style),
                Span::styled("─", style),
            ])
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Pane(Pane::Task));
    map.scroll_areas.push((area, Pane::Task));
    if overlay {
        let close_width = cells(width(CLOSE));
        let x = area.right().saturating_sub(close_width + 2);
        map.hits
            .add(Rect::new(x, area.y, close_width, 1), Hit::CloseOverlay);
    }
    if inner.height == 0 || inner.width == 0 {
        map.scroll_max.push((Pane::Task, 0));
        return;
    }
    let rows = rows(app, inner.width);
    let max = rows.len().saturating_sub(usize::from(inner.height));
    map.scroll_max.push((Pane::Task, max));
    draw_rows(frame, inner, rows, app.view.task_scroll.min(max), map);
}

/// The pane's rows at `width` for whatever is selected.
pub(crate) fn rows(app: &App, width: u16) -> Vec<Row> {
    let none = || {
        empty_lines(
            &[
                "No task selected.",
                "Click a crewmate to see its task here.",
            ],
            width,
        )
        .into_iter()
        .map(Row::plain)
        .collect()
    };
    let Some(agent) = app.view.selected_crew else {
        return none();
    };
    let AgentRef::Crewmate { task } = agent else {
        let label = app
            .snapshot
            .crew
            .iter()
            .find(|row| row.agent == agent)
            .map_or_else(
                || String::from("The Quartermaster"),
                |row| row.label.clone(),
            );
        let mut rows: Vec<Row> = empty_lines(&[&format!("{label} has no task of its own.")], width)
            .into_iter()
            .map(Row::plain)
            .collect();
        // A secondmate's session opens from here too, as a crewmate's does.
        if matches!(agent, AgentRef::Secondmate { .. }) {
            rows.push(Row::plain(Line::default()));
            rows.push(buttons(false, app.term.hover));
        }
        return rows;
    };
    match app.snapshot.tasks.iter().find(|t| t.task == task) {
        Some(detail) => detail_rows(detail, width, app.term.hover),
        // A task is selected (its id is the title), so "no task selected" would be wrong.
        None => empty_lines(&[&format!("{task}'s details haven't arrived yet.")], width)
            .into_iter()
            .map(Row::plain)
            .collect(),
    }
}

/// The task's rows: title, labelled rows, activity, buttons.
fn detail_rows(detail: &TaskDetail, max: u16, hover: Option<Hit>) -> Vec<Row> {
    let max_cells = usize::from(max);
    let mut out = Vec::new();
    let title = [(detail.title.clone(), theme::strong())];
    push_wrapped(&mut out, &title, 0, max_cells);
    for row in &detail.rows {
        let runs: Vec<(String, Style)> = row
            .value
            .iter()
            .map(|s| (s.text.clone(), theme::tone(s.tone)))
            .collect();
        match &row.label {
            Some(label) if max_cells > VALUE_AT => {
                let first = out.len();
                push_wrapped(&mut out, &runs, VALUE_AT, max_cells);
                // The label goes in front of the value's first row.
                let shown = truncate(label, LABEL_WIDTH);
                let pad = VALUE_AT - width(&shown);
                let lead = vec![
                    Span::styled(shown.clone(), theme::dim()),
                    Span::raw(" ".repeat(pad)),
                ];
                let row = &mut out[first];
                row.line.spans.splice(0..1, lead);
                if let Some(text) = &mut row.text {
                    text.text = format!("{shown}{}{}", " ".repeat(pad), text.text);
                    text.offset = 0;
                }
            }
            _ => push_wrapped(&mut out, &runs, 0, max_cells),
        }
    }
    if !detail.activity.is_empty() {
        out.push(Row::plain(Line::default()));
        for tool in &detail.activity {
            let text = truncate(&format!("▸ {tool}"), max_cells);
            out.push(Row::text(Line::styled(text, theme::text()), false));
        }
    }
    out.push(Row::plain(Line::default()));
    out.push(buttons(detail.running, hover));
    out
}

/// `Open session` and, while the task runs, `Stop`, one space apart.
fn buttons(running: bool, hover: Option<Hit>) -> Row {
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = 0;
    let mut labels = vec![("Open session", TaskButton::OpenSession)];
    if running {
        labels.push(("Stop", TaskButton::Stop));
    }
    for (label, which) in labels {
        if x > 0 {
            spans.push(Span::raw(" "));
            x += 1;
        }
        let hit = Hit::TaskButton(which);
        spans.push(button::span(label, hover == Some(hit)));
        let w = button::width_of(label);
        hits.push((x, w, hit));
        x += w;
    }
    Row {
        line: Line::from(spans),
        text: None,
        hits,
    }
}

/// Wraps `runs` to `max` cells with every row indented `indent` cells, as selectable rows.
fn push_wrapped(out: &mut Vec<Row>, runs: &[(String, Style)], indent: usize, max: usize) {
    for (i, row) in wrap_parts(runs, max.saturating_sub(indent))
        .into_iter()
        .enumerate()
    {
        let text: String = row.iter().map(|(t, _)| t.as_str()).collect();
        let mut spans = vec![Span::raw(" ".repeat(indent))];
        spans.extend(row.into_iter().map(|(t, s)| Span::styled(t, s)));
        out.push(Row {
            line: Line::from(spans),
            text: Some(RowText {
                offset: cells(indent),
                text,
                continues: i > 0,
            }),
            hits: Vec::new(),
        });
    }
}
