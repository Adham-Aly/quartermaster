//! The task pane: the selected crewmate's task, in place in the right column or as the folded
//! overlay.

use super::button;
use super::context_strip;
use super::conversation::{Row, RowText, draw_rows};
use super::tools;
use crate::app::App;
use crate::bridge::{empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane, TaskButton};
use crate::text::{cells, truncate, width, wrap_parts};
use crate::theme;
use qm_types::api::{AgentRef, TaskDetail, TaskRow, Tone};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
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
        Some(agent @ AgentRef::Crewmate { .. }) => {
            super::crew::crewmate_name(app, agent).to_owned()
        }
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
    let mut rows = rows(app, inner.width);
    let mut body = inner;
    // When the rows overflow, the buttons stay pinned to the bottom (a blank row above them) and
    // only the rest scrolls.
    let footer_at = rows.iter().position(|row| {
        row.hits
            .iter()
            .any(|(_, _, hit)| matches!(hit, Hit::TaskButton(_)))
    });
    if let Some(at) = footer_at
        && rows.len() > usize::from(inner.height)
    {
        let footer = rows.split_off(at);
        while rows
            .last()
            .is_some_and(|row| row.hits.is_empty() && row.line.width() == 0)
        {
            rows.pop();
        }
        let height = cells(footer.len() + 1).min(inner.height / 2);
        body.height -= height;
        let area = Rect {
            y: body.bottom() + 1,
            height: height.saturating_sub(1),
            ..inner
        };
        draw_rows(frame, area, footer, 0, map);
    }
    let max = rows.len().saturating_sub(usize::from(body.height));
    map.scroll_max.push((Pane::Task, max));
    draw_rows(frame, body, rows, app.view.task_scroll.min(max), map);
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
        // Its context and turn in full; the strip in the box's border has only their summary.
        let max = usize::from(width);
        let mut usage = Vec::new();
        for line in context_strip::usage_lines(app, context_strip::view_for(app, agent), max) {
            push_wrapped(&mut usage, &line, 0, max);
        }
        for line in context_strip::turn_lines(app, agent, max) {
            push_wrapped(&mut usage, &line, 0, max);
        }
        push_section(&mut rows, "Usage", usage, max);
        // A secondmate's session opens from here too, as a crewmate's does.
        if matches!(agent, AgentRef::Secondmate { .. }) {
            rows.push(Row::plain(Line::default()));
            rows.push(buttons(false, app.term.hover));
        }
        return rows;
    };
    match app.snapshot.tasks.iter().find(|t| t.task == task) {
        Some(detail) => detail_rows(app, agent, detail, width),
        // Selection is kept by its typed ID while the details load; the wording is generic.
        None => empty_lines(&["Task details haven't arrived yet."], width)
            .into_iter()
            .map(Row::plain)
            .collect(),
    }
}

/// The task's rows: its status and kind as chips, the bold title, then Usage, Routing,
/// Workspace and Recent sections built from the core's labelled rows, then the buttons. A row
/// with a label this build doesn't place keeps its label, after Workspace.
fn detail_rows(app: &App, agent: AgentRef, detail: &TaskDetail, max: u16) -> Vec<Row> {
    let hover = app.term.hover;
    let max_cells = usize::from(max);
    let mut out = Vec::new();
    if let Some(line) = chips(find(detail, "status"), &detail.kind, max_cells) {
        out.push(Row::plain(line));
        out.push(Row::plain(Line::default()));
    }
    let title = [(detail.title.clone(), theme::strong())];
    push_wrapped(&mut out, &title, 0, max_cells);
    push_section(
        &mut out,
        "Usage",
        usage(app, agent, detail, max_cells),
        max_cells,
    );
    push_section(&mut out, "Routing", routing(detail, max_cells), max_cells);
    push_section(
        &mut out,
        "Workspace",
        workspace(app, detail, max_cells),
        max_cells,
    );
    push_section(&mut out, "Recent", recent(detail, max_cells), max_cells);
    out.push(Row::plain(Line::default()));
    if detail.report_available {
        // Its own row keeps the action readable even in the narrow folded task pane.
        let hit = Hit::TaskButton(TaskButton::OpenReport);
        out.push(Row {
            line: Line::from(button::span("Open report", hover == Some(hit))),
            text: None,
            hits: vec![(0, button::width_of("Open report"), hit)],
        });
        out.push(Row::plain(Line::default()));
    }
    out.push(buttons(detail.running, hover));
    out
}

/// The core's row labelled `label`.
fn find<'a>(detail: &'a TaskDetail, label: &str) -> Option<&'a TaskRow> {
    detail
        .rows
        .iter()
        .find(|row| row.label.as_deref() == Some(label))
}

/// A core row's value as runs in their tones.
fn runs_of(row: &TaskRow) -> Vec<(String, Style)> {
    row.value
        .iter()
        .map(|s| (s.text.clone(), theme::tone(s.tone)))
        .collect()
}

/// Usage: live from the engine's passive readings (occupancy against the auto-compact point),
/// only while the core holds one, then the task's token totals, then the current or last turn.
fn usage(app: &App, agent: AgentRef, detail: &TaskDetail, max: usize) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(view) = context_strip::view_for(app, agent) {
        for line in context_strip::usage_lines(app, Some(view), max) {
            push_wrapped(&mut out, &line, 0, max);
        }
    }
    if let Some(row) = find(detail, "tokens") {
        // Worded as the turn below them: dim words, bright numbers, the cost bold, since it is
        // the number people look for.
        let mut runs = vec![("total · ".to_owned(), theme::dim())];
        for s in &row.value {
            if s.tone == Tone::Text {
                runs.extend(numbers_bright(&s.text));
            } else {
                runs.push((s.text.clone(), theme::tone(s.tone)));
            }
        }
        push_wrapped(&mut out, &runs, 0, max);
    }
    let turn = context_strip::turn_lines(app, agent, max);
    if !turn.is_empty() && !out.is_empty() {
        out.push(Row::plain(Line::default()));
    }
    for line in turn {
        push_wrapped(&mut out, &line, 0, max);
    }
    out
}

/// Routing: the model, account and tier after their glyphs, then why, italic.
fn routing(detail: &TaskDetail, max: usize) -> Vec<Row> {
    let mut out = Vec::new();
    for (label, glyph, style) in [
        ("model", "◈", theme::violet()),
        ("account", "◉", theme::dim()),
        ("tier", "▲", theme::warn()),
    ] {
        if let Some(row) = find(detail, label) {
            push_glyph(&mut out, glyph, style, &runs_of(row), max);
        }
    }
    for row in detail.rows.iter().filter(|row| row.label.is_none()) {
        let text: String = row.value.iter().map(|s| s.text.as_str()).collect();
        let why = text
            .strip_prefix("why: ")
            .map_or(text.as_str(), |why| why.trim_matches(['“', '”']));
        let style = theme::dim().add_modifier(Modifier::ITALIC);
        push_wrapped(&mut out, &[(why.to_owned(), style)], GLYPH_AT, max);
    }
    out
}

/// Workspace: the project, the branch or folder, verification, then any row this build doesn't
/// place, with its label.
fn workspace(app: &App, detail: &TaskDetail, max: usize) -> Vec<Row> {
    let mut out = Vec::new();
    if let Some(row) = find(detail, "project") {
        push_glyph(&mut out, "●", theme::warn(), &runs_of(row), max);
    }
    for label in ["branch", "folder"] {
        if let Some(row) = find(detail, label) {
            // `branch ← main`: the target dim, so the task's own branch stands out.
            let runs: Vec<(String, Style)> = row
                .value
                .iter()
                .flat_map(|s| match s.text.split_once(" ← ") {
                    Some((ours, target)) => vec![
                        (ours.to_owned(), theme::title()),
                        (format!(" ← {target}"), theme::dim()),
                    ],
                    None => vec![(s.text.clone(), theme::title())],
                })
                .collect();
            push_glyph(&mut out, "⌂", theme::dim(), &runs, max);
        }
    }
    if let Some(row) = find(detail, "verify") {
        let mut runs = vec![("verify ".to_owned(), theme::text())];
        runs.extend(row.value.iter().map(|s| (s.text.clone(), theme::dim())));
        push_glyph(&mut out, "✓", theme::accent(), &runs, max);
    }
    if let Some(row) = find(detail, "task") {
        let value: String = row.value.iter().map(|s| s.text.as_str()).collect();
        // Older snapshots used this row solely for IDs. New ones carry a friendly name
        // and collaborator count; keep that useful context verbatim.
        let legacy = value
            .split(" · ")
            .next()
            .is_some_and(|word| word.parse::<qm_types::ids::TaskLabelId>().is_ok());
        let value = if legacy {
            crate::presentation::Presenter(&app.snapshot).metadata(&value)
        } else {
            value
        };
        push_labelled(&mut out, Some("task"), &[(value, theme::text())], max);
    }
    for row in &detail.rows {
        if let Some(label) = row.label.as_deref()
            && !PLACED.contains(&label)
        {
            push_labelled(&mut out, Some(label), &runs_of(row), max);
        }
    }
    out
}

/// Recent: each tool call, its name in its family's colour and its target after it.
fn recent(detail: &TaskDetail, max: usize) -> Vec<Row> {
    // Short names (the built-ins) line up; a longer one just takes its own width.
    let name_width = detail
        .activity
        .iter()
        .map(|call| width(tools::split(call).0))
        .filter(|w| *w <= NAME_MAX)
        .max()
        .unwrap_or(0);
    detail
        .activity
        .iter()
        .map(|call| {
            let (name, target, result) = tools::split(call);
            let mut runs = vec![
                ("▸ ".to_owned(), theme::rule()),
                (name.to_owned(), tools::style(name)),
            ];
            if !target.is_empty() {
                let pad = name_width.saturating_sub(width(name)) + 1;
                runs.push((" ".repeat(pad), Style::default()));
                runs.push((target.to_owned(), theme::title()));
            }
            if let Some(result) = result {
                runs.push((format!("  → {result}"), theme::dim()));
            }
            Row::text(clip(runs, max), false)
        })
        .collect()
}

/// Labels handled by `detail_rows`, including the friendly task/collaborator metadata.
const PLACED: [&str; 10] = [
    "status", "tokens", "model", "account", "tier", "project", "branch", "folder", "verify", "task",
];

/// Where a glyph row's value starts: the glyph, then a space.
const GLYPH_AT: usize = 2;

/// Recent tool names this short are padded to the longest of them.
const NAME_MAX: usize = 8;

/// The status chip (`○ waits` in the warn chip, `● running 12m` on the fill) and the kind chip
/// (`plan`, in the code colour on the fill), one space apart; the kind goes first when both
/// don't fit.
fn chips(status: Option<&TaskRow>, kind: &str, max: usize) -> Option<Line<'static>> {
    // On the fill, a colour that doesn't read there moves to one that does.
    let on_fill = theme::on_selection;
    let mut spans = Vec::new();
    if let Some(row) = status {
        let words: String = row.value.iter().map(|s| s.text.as_str()).collect();
        let tone = row.value.first().map_or(Tone::Text, |s| s.tone);
        let (glyph, style) = match tone {
            Tone::Ok => ("●", on_fill(theme::ok())),
            Tone::Warn => ("○", theme::warn_chip()),
            Tone::Accent => ("○", on_fill(theme::accent())),
            _ => ("○", on_fill(theme::text())),
        };
        let chip = format!(" {glyph} {} ", words.trim());
        spans.push(Span::styled(truncate(&chip, max), style));
    }
    if !kind.is_empty() {
        let chip = format!(" {kind} ");
        let used: usize = spans.iter().map(|s| width(&s.content) + 1).sum();
        if used + width(&chip) <= max {
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(chip, on_fill(theme::code())));
        }
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

/// `in 2.5k · $2.48 · 14 turns` with its words dim, its numbers in the text colour and a cost
/// bold.
fn numbers_bright(text: &str) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for (i, word) in text.split(' ').enumerate() {
        let style = if word.starts_with('$') {
            theme::strong()
        } else if word.starts_with(|c: char| c.is_ascii_digit()) {
            theme::text()
        } else {
            theme::dim()
        };
        let word = if i == 0 {
            word.to_owned()
        } else {
            format!(" {word}")
        };
        match out.last_mut() {
            Some((last, s)) if *s == style => last.push_str(&word),
            _ => out.push((word, style)),
        }
    }
    out
}

/// A section: a blank row, `─ Heading ───` across the pane, then its rows; nothing when it has
/// none.
fn push_section(out: &mut Vec<Row>, heading: &str, rows: Vec<Row>, max: usize) {
    if rows.is_empty() {
        return;
    }
    out.push(Row::plain(Line::default()));
    let lead = format!("─ {heading} ");
    let rest = max.saturating_sub(width(&lead));
    out.push(Row::plain(Line::from(vec![
        Span::styled("─ ", theme::rule()),
        Span::styled(format!("{heading} "), theme::dim()),
        Span::styled("─".repeat(rest), theme::rule()),
    ])));
    out.extend(rows);
}

/// Pushes `runs` wrapped after a one-cell `glyph` and a space.
fn push_glyph(out: &mut Vec<Row>, glyph: &str, style: Style, runs: &[(String, Style)], max: usize) {
    let first = out.len();
    push_wrapped(out, runs, GLYPH_AT, max);
    if let Some(row) = out.get_mut(first) {
        row.line.spans.splice(
            0..1,
            [Span::styled(glyph.to_owned(), style), Span::raw(" ")],
        );
    }
}

/// `runs` on one line of at most `max` cells, ending in `…` when cut.
fn clip(runs: Vec<(String, Style)>, max: usize) -> Line<'static> {
    let total: usize = runs.iter().map(|(t, _)| width(t)).sum();
    if total <= max {
        return Line::from(
            runs.into_iter()
                .map(|(t, s)| Span::styled(t, s))
                .collect::<Vec<_>>(),
        );
    }
    let mut left = max;
    let mut spans = Vec::new();
    for (text, style) in runs {
        if left == 0 {
            break;
        }
        let w = width(&text);
        if w < left {
            left -= w;
            spans.push(Span::styled(text, style));
        } else {
            spans.push(Span::styled(truncate(&text, left), style));
            left = 0;
        }
    }
    Line::from(spans)
}

/// `▸ Open session`, bold on the accent, and, while the task runs, `■ Stop` in the error
/// colour, one space apart.
fn buttons(running: bool, hover: Option<Hit>) -> Row {
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut x = 0;
    let open = Hit::TaskButton(TaskButton::OpenSession);
    spans.push(button::primary(OPEN, hover == Some(open)));
    let w = button::width_of(OPEN);
    hits.push((x, w, open));
    x += w;
    if running {
        spans.push(Span::raw(" "));
        x += 1;
        let stop = Hit::TaskButton(TaskButton::Stop);
        spans.push(button::danger(STOP, hover == Some(stop)));
        hits.push((x, button::width_of(STOP), stop));
    }
    Row {
        line: Line::from(spans),
        text: None,
        hits,
    }
}

const OPEN: &str = "▸ Open session";
const STOP: &str = "■ Stop";

/// Pushes a row with `label` in the label column and `runs` wrapped beside it.
fn push_labelled(
    out: &mut Vec<Row>,
    label: Option<&str>,
    runs: &[(String, Style)],
    max_cells: usize,
) {
    match label {
        Some(label) if max_cells > VALUE_AT => {
            let first = out.len();
            push_wrapped(out, runs, VALUE_AT, max_cells);
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
        _ => push_wrapped(out, runs, 0, max_cells),
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

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::sample;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn the_core_rows_are_grouped_into_sections_under_chips() {
        let mut app = App::new();
        sample::install(&mut app);
        let task = app.snapshot.tasks[0].task;
        app.view.selected_crew = Some(AgentRef::Crewmate { task });
        let rows = rows(&app, 40);
        let shown = texts(&rows);
        assert_eq!(shown[0], " ● running 12m   code ");
        let at = |words: &str| {
            shown
                .iter()
                .position(|s| s.starts_with(words))
                .unwrap_or_else(|| panic!("{words}: {shown:#?}"))
        };
        assert!(at("─ Usage ─") < at("─ Routing ─"));
        assert!(at("─ Routing ─") < at("─ Workspace ─"));
        assert!(at("─ Workspace ─") < at("─ Recent ─"));
        assert_eq!(shown[at("◈ ")], "◈ sonnet-5-5");
        assert_eq!(
            shown[at("◈ ") + 3],
            "  Fixing from the confirmed reproduction"
        );
        assert_eq!(shown[at("● api")], "● api-server");
        assert_eq!(shown[at("✓ ")], "✓ verify Default (standard)");
        assert_eq!(shown[at("▸ Read")], "▸ Read src/auth/refresh.rs");
        assert_eq!(shown[at("▸ delivery")], "▸ delivery_check");
        // No row keeps the old label column for a label the sections place.
        assert!(!shown.iter().any(|s| s.starts_with("status")));
        let buttons = rows.last().unwrap();
        assert_eq!(
            texts(std::slice::from_ref(buttons))[0],
            " ▸ Open session   ■ Stop "
        );
        assert_eq!(buttons.hits.len(), 2);
    }

    #[test]
    fn title_and_loading_are_friendly_without_changing_detail_content() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        sample::install(&mut app);
        let task = app.snapshot.tasks[0].task;
        let agent = AgentRef::Crewmate { task };
        app.view.selected_crew = Some(agent);
        for overlay in [false, true] {
            let mut terminal = Terminal::new(TestBackend::new(70, 30)).unwrap();
            let mut map = FrameMap::default();
            terminal
                .draw(|f| draw(f, f.area(), &app, &mut map, overlay))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let border: String = (0..70).map(|x| buffer[(x, 0)].symbol()).collect();
            assert!(border.contains("Token refresh race"), "{border}");
            assert!(!border.contains(&task.to_string()));
            assert!(
                map.hits
                    .rect_of(Hit::TaskButton(TaskButton::OpenSession))
                    .is_some()
            );
            assert!(
                map.hits
                    .rect_of(Hit::TaskButton(TaskButton::Stop))
                    .is_some()
            );
            if overlay {
                assert!(map.hits.rect_of(Hit::CloseOverlay).is_some());
            }
        }
        app.snapshot.tasks[0].title = "Investigate T-0042 and C-0042".to_owned();
        app.snapshot.tasks[0].rows.push(TaskRow::new(
            Some("task"),
            vec![qm_types::api::Styled::new(
                "T-0099 · with C-0077",
                Tone::Text,
            )],
        ));
        app.snapshot.tasks[0].rows.push(TaskRow::new(
            Some("folder"),
            vec![qm_types::api::Styled::new(
                "/repo/T-0042/C-0042",
                Tone::Text,
            )],
        ));
        app.snapshot.tasks[0]
            .activity
            .push("Read /repo/T-0042/C-0042 → C-0042 kept".to_owned());
        let shown = texts(&rows(&app, 100)).join("\n");
        assert!(shown.contains("Investigate T-0042 and C-0042"));
        assert!(shown.contains("/repo/T-0042/C-0042"));
        assert!(shown.contains("C-0042 kept"));
        assert!(!shown.contains("T-0099") && !shown.contains("C-0077"));
        // Render-only hiding: the ID metadata remains available in the snapshot.
        assert!(find(&app.snapshot.tasks[0], "task").is_some());
        app.snapshot.crew.clear();
        assert_eq!(
            super::super::crew::crewmate_name(&app, agent),
            "Investigate T-0042 and C-0042"
        );
        app.snapshot.tasks.clear();
        assert_eq!(
            texts(&rows(&app, 70)),
            ["Task details haven't arrived yet."]
        );
        app.snapshot.crew.clear();
        assert_eq!(super::super::crew::crewmate_name(&app, agent), "Crewmate");
    }

    #[test]
    fn an_unknown_label_keeps_its_label() {
        let mut app = App::new();
        sample::install(&mut app);
        let task = app.snapshot.tasks[0].task;
        app.snapshot.tasks[0].rows.push(TaskRow::new(
            Some("queue"),
            vec![qm_types::api::Styled::new("second", Tone::Text)],
        ));
        app.view.selected_crew = Some(AgentRef::Crewmate { task });
        let shown = texts(&rows(&app, 40));
        assert!(shown.contains(&"queue    second".to_owned()), "{shown:#?}");
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;
    use crate::hit::Pane;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use qm_types::api::Command;
    use qm_types::ids::TaskId;
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Instant;

    #[test]
    fn report_action_is_persistent_narrow_safe_and_has_mouse_and_keyboard_controls() {
        let task = TaskId::new(42).unwrap();
        let mut app = App::new();
        let mut detail = TaskDetail::new(task, "Research results");
        detail.report_available = true;
        app.snapshot.tasks.push(detail);
        app.view.selected_crew = Some(AgentRef::Crewmate { task });
        app.focus = Pane::Task;
        for (w, h) in [(150, 45), (120, 40), (119, 40), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            // Draw the same persistent task panel at its narrowest supported width.
            let mut map = FrameMap::default();
            terminal
                .draw(|frame| draw(frame, Rect::new(0, 0, 24, h), &app, &mut map, true))
                .unwrap();
            let hit = Hit::TaskButton(TaskButton::OpenReport);
            let area = map.hits.rect_of(hit).expect("visible report action");
            assert!(area.right() <= 24);
            app.term.drawn = map;
            app.on_event(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                }),
                Instant::now(),
            );
            assert_eq!(app.take_commands(), [Command::OpenReport { task }]);
            app.on_event(
                Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
                Instant::now(),
            );
            assert_eq!(app.take_commands(), [Command::OpenReport { task }]);
        }
        app.snapshot.tasks[0].report_available = false;
        assert!(!rows(&app, 24).iter().any(|row| {
            row.hits
                .iter()
                .any(|(_, _, hit)| *hit == Hit::TaskButton(TaskButton::OpenReport))
        }));
        app.open_selected_report();
        assert_eq!(app.take_commands(), [] as [Command; 0]);
        app.view.selected_crew = Some(AgentRef::Quartermaster);
        app.open_selected_report();
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }
}
