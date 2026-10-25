//! Drawing a worker's session in the Bridge's middle panel, in place of the Quartermaster's
//! conversation and input (the projects, crew and task columns stay):
//!
//! ```text
//! ╭─ C-0042 · fix(auth): token refresh race ─────────────────────  ← Quartermaster ─╮
//! │ api-server · sonnet-5-5 · supervisor: Quartermaster                     (dim)   │
//! │ Talking to this crewmate directly. You override its supervisor; it will  (warn) │
//! │ reconcile.                                                                      │
//! │                                                                                 │
//! │ [from supervisor] Brief · C-0042 · fix(auth): token refresh race  Expand        │
//! │                                                                                 │
//! │ Reading AGENTS.md and CONTRIBUTING.md, then calling delivery_check before any   │
//! │ edit.                                                                           │
//! │   ▸ 3 tool calls                                                    (dim)       │
//! │ ╭─ you (direct) ──────────────────────────────────────────────────────────────╮ │
//! │ │ A unit test that reproduces the race is enough evidence here.               │ │
//! │ ╰─────────────────────────────────────────────────────────────────────────────╯ │
//! ╰─ running 14m · job J3 idle ─────────────────────────────────────── Interrupt ───╯
//! ╭─────────────────────────────────────────────────────────────────────────────────╮
//! │ › message C-0042 directly…                                                      │
//! ╰─────────────────────────────────────────────────────────────────────────────────╯
//! ```
//!
//! The box's title names the agent and its task; `← Quartermaster` at its right goes back to the
//! conversation, as `Esc` does. The note and the banner stay at the top while the lines scroll
//! under them; the bottom border carries the session's status and, while a turn runs,
//! `Interrupt`. The Bridge's top bar and status bar stay as they are.
//!
//! A secondmate's session has the same frame: its charter's first line shows dim in the banner's
//! row, and the box holds its conversation, drawn by the Bridge's chat widgets.

use super::{BANNER, SessionHit};
use crate::app::App;
use crate::bridge::{pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{cells, truncate, width, wrap, wrap_rows, wrap_styled};
use crate::theme;
use crate::widgets::conversation::{self, Row, RowText, draw_rows, prompt_box};
use crate::widgets::{button, input_box, markdown, spinner, stream, tools};
use qm_types::api::{AgentRef, SessionLine, SessionLineKind, SessionView};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The title's way back to the Quartermaster's conversation.
pub(crate) const BACK: &str = "← Quartermaster";
/// The bottom border's `Interrupt` button.
const INTERRUPT: &str = "Interrupt";

/// Draws `agent`'s session into the middle column `area`: its box above your input.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent: AgentRef,
    map: &mut FrameMap,
) {
    let view = app.session_view();
    let input_height = input_box::height(app, area.width);
    let [session_area, input_area] = crate::widgets::context_strip::split(area, input_height);
    session_pane(frame, session_area, app, agent, view, map);
    input(frame, input_area, app, agent, view, map);
}

/// How the agent is named: `C-0042`, `S-02`.
pub(crate) fn agent_label(agent: AgentRef) -> String {
    match agent {
        AgentRef::Crewmate { task } => task.to_string(),
        AgentRef::Secondmate { id } => id.to_string(),
        AgentRef::Quartermaster => "Quartermaster".to_owned(),
    }
}

/// The box's title: `C-0042 · fix(auth): token refresh race`.
fn title(app: &App, agent: AgentRef, view: Option<&SessionView>) -> String {
    let words = view
        .map(|v| v.title.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            // Before the core's view arrives: the crew row's words.
            app.snapshot
                .tasks
                .iter()
                .find(|t| AgentRef::Crewmate { task: t.task } == agent)
                .map(|t| t.title.clone())
        });
    match words {
        Some(words) => format!("{} · {words}", agent_label(agent)),
        None => agent_label(agent),
    }
}

/// The session's box: its title with `← Quartermaster`, the note and banner, its lines pinned to
/// the newest once they overflow, and the status and `Interrupt` in its bottom border.
fn session_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent: AgentRef,
    view: Option<&SessionView>,
    map: &mut FrameMap,
) {
    let style = pane_style(app, Pane::Session);
    // `← Quartermaster` sits in the top border, one `─` from the corner; the title gets what is
    // left, with at least one `─` between them.
    let back = Hit::Session(SessionHit::Back);
    let back_width = button::width_of(BACK);
    let back_x = area.right().saturating_sub(back_width + 2);
    let room = usize::from(back_x.saturating_sub(area.x + 6));
    let block = pane_block(&truncate(&title(app, agent, view), room), style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Pane(Pane::Session));
    map.scroll_areas.push((area, Pane::Session));
    if area.width > back_width + 8 && area.height > 1 {
        let rect = Rect::new(back_x, area.y, back_width, 1);
        frame.render_widget(
            Line::from(button::span(BACK, app.term.hover == Some(back))),
            rect,
        );
        map.hits.add(rect, back);
    }
    if area.height > 1 {
        bottom_border(
            frame,
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
            app,
            agent,
            view,
            map,
        );
    }
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let rows_area = header(frame, inner, agent, view);
    map.session_rows = Some(rows_area);
    let lines = view.map_or(&[][..], |v| v.lines.as_slice());
    let chats = view.map_or(&[][..], |v| v.conversation.as_slice());
    if lines.is_empty() && chats.is_empty() {
        let text = if view.is_some() {
            "Nothing in this session yet."
        } else {
            "Opening the session…"
        };
        let y = rows_area.y + rows_area.height / 2;
        frame.render_widget(
            Paragraph::new(Line::styled(text, theme::dim())).alignment(Alignment::Center),
            Rect::new(rows_area.x, y, rows_area.width, 1),
        );
        return;
    }
    let notes = crate::widgets::context_strip::chat_notes(app, agent);
    let compacting = crate::widgets::context_strip::compacting_row(app, agent);
    let mut rows = if chats.is_empty() {
        rows(lines, &notes, rows_area.width, app, compacting.is_none())
    } else {
        // A secondmate's conversation, from its side: its own replies under its name, the
        // Quartermaster's messages under the Quartermaster's.
        let name = view.map_or_else(String::new, |v| agent_label(v.agent));
        let voices = conversation::Voices {
            agent: &name,
            supervisor: "quartermaster",
        };
        let mut rows = conversation::rows(chats, &notes, rows_area.width, &voices);
        if compacting.is_none() && view.is_some_and(|v| v.thinking) {
            rows.extend(conversation::thinking_rows(chats, app, agent));
        }
        rows
    };
    // In place of the busy row: what the agent is doing now.
    if let Some(row) = compacting {
        rows.push(Row::plain(Line::default()));
        rows.push(row);
    }
    let visible = usize::from(rows_area.height);
    let max = rows.len().saturating_sub(visible);
    let offset = app.session_scroll().min(max);
    let start = rows.len().saturating_sub(visible + offset);
    map.scroll_max.push((Pane::Session, max));
    draw_rows(frame, rows_area, rows, start, map);
}

/// The rows above the lines: the note (dim), then the banner in the warn colour on the terminal's
/// background (on the 16-colour bright-black fill it can't be read in a light theme) or, for a
/// secondmate, its charter's first line (dim), then a blank row. They never take more than a
/// third of the box: the note goes first, then the rest. Returns what is left for the lines.
fn header(frame: &mut Frame<'_>, inner: Rect, agent: AgentRef, view: Option<&SessionView>) -> Rect {
    let max = usize::from(inner.width);
    let note = view
        .map(|v| v.note.as_str())
        .filter(|n| !n.is_empty())
        .map(|note| Line::styled(truncate(note, max), theme::dim()));
    let banner: Vec<Line<'static>> =
        match view.map(|v| v.subtitle.as_str()).filter(|s| !s.is_empty()) {
            Some(subtitle) => vec![Line::styled(truncate(subtitle, max), theme::dim())],
            // The banner is a warning, so it wraps rather than losing its end.
            None if matches!(agent, AgentRef::Crewmate { .. }) => wrap(BANNER, max)
                .into_iter()
                .map(|row| Line::styled(row, theme::warn()))
                .collect(),
            None => Vec::new(),
        };
    let fits = |n: usize| n > 0 && (cells(n) + 1) * 3 <= inner.height;
    let texts: Vec<Line<'static>> = if fits(banner.len() + usize::from(note.is_some())) {
        note.into_iter().chain(banner).collect()
    } else if fits(banner.len()) {
        banner
    } else {
        return inner;
    };
    let used = cells(texts.len()) + 1;
    frame.render_widget(
        Paragraph::new(texts),
        Rect {
            height: used - 1,
            ..inner
        },
    );
    Rect {
        y: inner.y + used,
        height: inner.height - used,
        ..inner
    }
}

/// `╰─ running 14m · job J3 idle ── ━━━━─── 61% context · 3 calls ─ Interrupt ─╯`: the
/// session's status, dim, the context strip right-aligned, and while a turn runs the `Interrupt`
/// button, one `─` from the corner.
fn bottom_border(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent: AgentRef,
    view: Option<&SessionView>,
    map: &mut FrameMap,
) {
    let style = pane_style(app, Pane::Session);
    let running = view.is_some_and(|v| v.turn_running);
    let mut right = area.right().saturating_sub(2);
    let mut strip_end = area.right();
    if running {
        let w = button::width_of(INTERRUPT);
        if area.width > w + 8 {
            let hit = Hit::Session(SessionHit::Interrupt);
            right -= w;
            strip_end = right + 1;
            let rect = Rect::new(right, area.y, w, 1);
            frame.render_widget(
                Line::from(button::span(INTERRUPT, app.term.hover == Some(hit))),
                rect,
            );
            map.hits.add(rect, hit);
        }
    }
    let mut used = 0;
    if let Some(status) = view.map(|v| v.status.as_str()).filter(|s| !s.is_empty()) {
        // `╰─ ` before it, ` ─` after it at the least.
        let room = usize::from(right.saturating_sub(area.x + 5));
        if room >= 4 {
            let text = truncate(status, room);
            let line = Line::from(vec![
                Span::styled(" ", style),
                Span::styled(text, theme::dim()),
                Span::styled(" ", style),
            ]);
            used = cells(line.width());
            frame.render_widget(line, Rect::new(area.x + 2, area.y, used, 1));
        }
    }
    let strip = Rect {
        width: strip_end.saturating_sub(area.x),
        ..area
    };
    // One `─` between the status and the strip.
    crate::widgets::context_strip::draw_in_border(frame, strip, 2 + used + 1, app, agent);
}

/// The rows a session's lines take at `max` cells.
struct Rows {
    rows: Vec<Row>,
}

impl Rows {
    /// A blank row, unless the last one is blank or there is none yet.
    fn blank(&mut self) {
        let last_blank = self
            .rows
            .last()
            .is_none_or(|r| r.text.is_none() && r.hits.is_empty() && r.line.width() == 0);
        if !last_blank {
            self.rows.push(Row::plain(Line::default()));
        }
    }

    /// Rows of `text` in `style`, indented by `indent` cells, the first after `lead` (as wide as
    /// the indent).
    fn wrapped(&mut self, text: &str, max: u16, indent: u16, lead: &str, style: Style) {
        let room = usize::from(max.saturating_sub(indent));
        for (i, (row, continues)) in wrap_rows(text, room).into_iter().enumerate() {
            let pad = if i == 0 {
                lead.to_owned()
            } else {
                " ".repeat(usize::from(indent))
            };
            self.rows.push(Row {
                line: Line::from(vec![
                    Span::styled(pad, style),
                    Span::styled(row.clone(), style),
                ]),
                text: Some(RowText {
                    offset: indent,
                    text: row,
                    continues,
                }),
                hits: Vec::new(),
            });
        }
    }
}

/// Every line's rows: a blank row after a supervisor or app line, and before and after your
/// message, which sits in a box. A run of tool calls is one line, `▸ 3 tool calls`, until you
/// expand it. While the worker's turn runs, the busy indicator comes last, until a line of its
/// text shows in its place.
pub(crate) fn rows(
    lines: &[SessionLine],
    notes: &[&qm_types::api::CompactionNote],
    max: u16,
    app: &App,
    busy_row: bool,
) -> Vec<Row> {
    let mut notes = notes;
    let mut out = Rows { rows: Vec::new() };
    let running = app.session_view().is_some_and(|v| v.turn_running);
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        i += 1;
        let noted = crate::widgets::context_strip::notes_before(
            &mut notes,
            Some(line.at),
            max,
            out.rows.is_empty(),
        );
        if !noted.is_empty() {
            out.rows.extend(noted);
            out.blank();
        }
        match line.kind {
            SessionLineKind::Supervisor | SessionLineKind::App => {
                marked(&mut out, line, max, app);
                out.blank();
            }
            SessionLineKind::Tool => {
                let run = lines[i - 1..]
                    .iter()
                    .take_while(|l| l.kind == SessionLineKind::Tool)
                    .count();
                let group = &lines[i - 1..i - 1 + run];
                i += run - 1;
                // Still counting while the turn runs and nothing has come after it.
                let live = running && i == lines.len();
                let until = lines.get(i).map(|l| l.at);
                tool_group(&mut out, group, live, until, max, app);
            }
            SessionLineKind::You => {
                out.blank();
                let title = vec![
                    Span::styled("you", theme::strong()),
                    Span::styled(" (direct)", theme::dim()),
                ];
                if let Some(boxed) = prompt_box(title.clone(), None, &line.text, max) {
                    out.rows.extend(boxed);
                } else {
                    out.rows.push(Row::text(Line::from(title), false));
                    out.wrapped(&line.text, max, 0, "", theme::text());
                }
                out.blank();
            }
            // The worker's text, and any kind a later build adds.
            _ => {
                // A block still arriving shows its whole lines; until it has one, the busy
                // indicator below stands in for it.
                let committed = stream::committed(&line.text, line.streaming);
                if !committed.is_empty() || !line.streaming {
                    let body = committed.strip_suffix('\n').unwrap_or(committed);
                    // Markdown, drawn as the Quartermaster's chat draws its replies.
                    for (line, continues) in markdown::rows(body, usize::from(max)) {
                        out.rows.push(Row::text(line, continues));
                    }
                }
            }
        }
    }
    let writing = lines
        .last()
        .is_some_and(|l| l.streaming && !stream::committed(&l.text, true).is_empty());
    let first = out.rows.is_empty();
    out.rows.extend(crate::widgets::context_strip::notes_before(
        &mut notes, None, max, first,
    ));
    if busy_row && running && !writing {
        let spinner = app
            .session_view()
            .map_or(spinner::Spinner::Helm, |v| spinner::Spinner::of(v.agent));
        // The turn's prompt: the last line the worker didn't write.
        let since = lines
            .iter()
            .rev()
            .find(|l| !matches!(l.kind, SessionLineKind::Agent | SessionLineKind::Tool))
            .map(|l| l.at);
        out.rows.push(Row::plain(spinner::line(
            app,
            spinner,
            &spinner::verb(spinner, since),
        )));
    }
    out.rows
}

/// A run of tool calls on one row: `▸ Read 4  Bash 3        7 calls · 14s`, each tool's name in
/// its colour with how many times it was called (the arrow dim, the text colour under the
/// pointer), and on the right how many calls and how long from the first to whatever came next
/// (`until`); `…` while `live`. A click shows each call under `▾`, and another hides them.
fn tool_group(
    out: &mut Rows,
    group: &[SessionLine],
    live: bool,
    until: Option<jiff::Timestamp>,
    max: u16,
    app: &App,
) {
    let Some(first) = group.first() else {
        return;
    };
    let hit = Hit::Session(SessionHit::ToolGroup(first.id));
    let expanded = app.is_tool_group_expanded(first.id);
    let n = group.len();
    let mut summary = format!("{n} call{}", if n == 1 { "" } else { "s" });
    if live {
        summary.push('…');
    } else if let Some(until) = until {
        let secs = until
            .as_second()
            .saturating_sub(first.at.as_second())
            .max(0);
        summary.push_str(" · ");
        summary.push_str(&duration(secs));
    }
    let arrow = if expanded { "▾" } else { "▸" };
    let arrow_style = if app.term.hover == Some(hit) {
        theme::text()
    } else {
        theme::dim()
    };
    // Each tool once, in the order first called, with its count.
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for line in group {
        let (name, _, _) = tools::split(&line.text);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let inner = usize::from(max.saturating_sub(2));
    // The arrow and its space; two cells at least before the summary.
    let mut room = inner.saturating_sub(2 + 2 + width(&summary));
    let mut chips: Vec<Span<'static>> = Vec::new();
    let mut text = format!("{arrow} ");
    for (k, (name, count)) in counts.iter().enumerate() {
        let chip = format!("{name} {count}");
        let gap = if chips.is_empty() { 0 } else { 2 };
        let left = counts.len() - k - 1;
        // Room for this chip, and for `+N` after it if more follow.
        let more = if left > 0 {
            2 + width(&format!("+{left}"))
        } else {
            0
        };
        if gap + width(&chip) + more > room {
            let rest = format!("+{}", counts.len() - k);
            if !chips.is_empty() && 2 + width(&rest) <= room {
                chips.push(Span::raw("  "));
                text.push_str("  ");
                chips.push(Span::styled(rest.clone(), theme::dim()));
                text.push_str(&rest);
            }
            break;
        }
        room -= gap + width(&chip);
        if gap > 0 {
            chips.push(Span::raw("  "));
            text.push_str("  ");
        }
        chips.push(Span::styled((*name).to_owned(), tools::style(name)));
        chips.push(Span::styled(format!(" {count}"), theme::dim()));
        text.push_str(&chip);
    }
    let pad = inner.saturating_sub(width(&text) + width(&summary)).max(2);
    let summary = truncate(&summary, inner.saturating_sub(width(&text) + 2));
    text.push_str(&" ".repeat(pad));
    text.push_str(&summary);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(arrow, arrow_style),
        Span::raw(" "),
    ];
    spans.extend(chips);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(summary, theme::dim()));
    out.rows.push(Row {
        line: Line::from(spans),
        hits: vec![(2, cells(width(&text)), hit)],
        text: Some(RowText {
            offset: 2,
            text,
            continues: false,
        }),
    });
    if expanded {
        for (k, line) in group.iter().enumerate() {
            let branch = if k + 1 == group.len() { "└ " } else { "├ " };
            call_rows(out, &line.text, branch, max);
        }
    }
}

/// One call under an expanded run: `├ Bash cargo test auth  → 41 passed`, the name in its tool's
/// colour, the target in the text colour and the result dim, wrapped under the name.
fn call_rows(out: &mut Rows, text: &str, branch: &str, max: u16) {
    let (name, target, result) = tools::split(text);
    let mut runs = vec![(name.to_owned(), tools::style(name))];
    if !target.is_empty() {
        runs.push((format!(" {target}"), theme::text()));
    }
    if let Some(result) = result {
        runs.push((format!("  → {result}"), theme::dim()));
    }
    let indent = 6;
    for (i, row) in wrap_styled(&runs, usize::from(max.saturating_sub(indent)))
        .into_iter()
        .enumerate()
    {
        let lead = if i == 0 {
            vec![
                Span::raw("    "),
                Span::styled(branch.to_owned(), theme::rule()),
            ]
        } else {
            vec![Span::raw(" ".repeat(usize::from(indent)))]
        };
        let row_text: String = row.iter().map(|(t, _)| t.as_str()).collect();
        let mut spans = lead;
        spans.extend(row.into_iter().map(|(t, style)| Span::styled(t, style)));
        out.rows.push(Row {
            line: Line::from(spans),
            text: Some(RowText {
                offset: indent,
                text: row_text,
                continues: i > 0,
            }),
            hits: Vec::new(),
        });
    }
}

/// A run's length in one short word: `4s`, `2m`, `1h`.
fn duration(secs: i64) -> String {
    match secs {
        ..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3_600),
    }
}

/// `[from supervisor] text  Expand`, and the detail under it when expanded.
fn marked(out: &mut Rows, line: &SessionLine, max: u16, app: &App) {
    let label = if line.kind == SessionLineKind::Supervisor {
        "[from supervisor]"
    } else {
        "[from app]"
    };
    let runs = vec![
        (label.to_owned(), theme::blue()),
        (" ".to_owned(), theme::text()),
        (line.text.clone(), theme::text()),
    ];
    let wrapped = wrap_styled(&runs, usize::from(max));
    let count = wrapped.len();
    for (i, row) in wrapped.into_iter().enumerate() {
        let text: String = row.iter().map(|(t, _)| t.as_str()).collect();
        let mut spans: Vec<Span<'static>> = row
            .into_iter()
            .map(|(t, style)| Span::styled(t, style))
            .collect();
        let mut hits = Vec::new();
        if i + 1 == count
            && line.detail.is_some()
            && let Some(button) = expand_button(line, app)
        {
            let used = cells(width(&text));
            let w = button::width_of(button.0);
            if used + 2 + w <= max {
                spans.push(Span::raw("  "));
                spans.push(button::span(button.0, app.term.hover == Some(button.1)));
                hits.push((used + 2, w, button.1));
            } else {
                out.rows.push(Row {
                    line: Line::from(spans),
                    text: Some(RowText {
                        offset: 0,
                        text,
                        continues: i > 0,
                    }),
                    hits: Vec::new(),
                });
                out.rows.push(Row {
                    line: Line::from(button::span(button.0, app.term.hover == Some(button.1))),
                    text: None,
                    hits: vec![(0, w, button.1)],
                });
                continue;
            }
        }
        out.rows.push(Row {
            line: Line::from(spans),
            text: Some(RowText {
                offset: 0,
                text,
                continues: i > 0,
            }),
            hits,
        });
    }
    if let Some(detail) = &line.detail
        && app.is_expanded(line.id)
    {
        // The detail is Markdown (a brief, a report): drawn as the conversation draws the
        // Quartermaster's replies, two cells in.
        for (row, continues) in markdown::rows(detail, usize::from(max.saturating_sub(2))) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row.spans);
            out.rows.push(Row {
                line: Line::from(spans),
                text: Some(RowText {
                    offset: 2,
                    text,
                    continues,
                }),
                hits: Vec::new(),
            });
        }
    }
}

/// The `Expand` or `Collapse` button of a line with a detail.
fn expand_button(line: &SessionLine, app: &App) -> Option<(&'static str, Hit)> {
    line.detail.as_ref()?;
    let label = if app.is_expanded(line.id) {
        "Collapse"
    } else {
        "Expand"
    };
    Some((label, Hit::Session(SessionHit::Expand(line.id))))
}

/// Your message to the worker: the shared prompt field, the Accent border while focused; the
/// reason, dim, once the session takes no more text.
fn input(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent: AgentRef,
    view: Option<&SessionView>,
    map: &mut FrameMap,
) {
    let open = app.session_open();
    let focused = app.focus == Pane::Input;
    let placeholder = view
        .map(|v| v.placeholder.clone())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| match agent {
            AgentRef::Secondmate { id } => format!("{id}'s session isn't available yet."),
            _ => format!("message {} directly…", agent_label(agent)),
        });
    let field = input_box::Field {
        placeholder,
        open,
        focused,
        accent: open && (focused || !app.draft.text.is_empty()),
    };
    input_box::draw_field(frame, area, app, &field, map);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn tool(id: u64, text: &str) -> SessionLine {
        SessionLine::new(id, SessionLineKind::Tool, jiff::Timestamp::UNIX_EPOCH, text)
    }

    #[test]
    fn a_run_of_tool_calls_is_one_line_that_counts_up_while_the_turn_runs() {
        let mut app = App::new();
        sample::install_session(&mut app);
        let mut lines = app.session_view().unwrap().lines.clone();
        lines.truncate(8);
        lines.push(tool(9, "Read a"));
        let shown = texts(&rows(&lines, &[], 80, &app, true));
        let first = shown
            .iter()
            .find(|s| s.starts_with("  ▸ Read 1  delivery_check 1  Bash 1 "))
            .expect("the first run");
        assert!(first.ends_with(" 3 calls · 2m"), "{shown:#?}");
        assert!(
            shown
                .iter()
                .any(|s| s.starts_with("  ▸ Read 1 ") && s.ends_with(" 1 call…")),
            "{shown:#?}"
        );
        assert!(!shown.iter().any(|s| s.contains("Read AGENTS.md")));
        lines.push(tool(10, "Edit b"));
        let shown = texts(&rows(&lines, &[], 80, &app, true));
        assert!(
            shown
                .iter()
                .any(|s| s.starts_with("  ▸ Read 1  Edit 1 ") && s.ends_with(" 2 calls…")),
            "{shown:#?}"
        );
        // Once the turn ends, the count stops.
        app.snapshot.session.as_mut().unwrap().turn_running = false;
        let shown = texts(&rows(&lines, &[], 80, &app, true));
        assert!(
            shown
                .iter()
                .any(|s| s.starts_with("  ▸ Read 1  Edit 1 ") && s.ends_with(" 2 calls")),
            "{shown:#?}"
        );
    }

    #[test]
    fn a_tool_group_stays_expanded_by_its_key_as_calls_arrive() {
        let mut app = App::new();
        sample::install_session(&mut app);
        let mut lines = app.session_view().unwrap().lines.clone();
        lines.truncate(8);
        lines.push(tool(9, "Read a"));
        app.toggle_tool_group(9);
        lines.push(tool(10, "Edit b"));
        let shown = texts(&rows(&lines, &[], 80, &app, true));
        let at = shown
            .iter()
            .position(|s| s.starts_with("  ▾ Read 1  Edit 1 ") && s.ends_with(" 2 calls…"))
            .unwrap_or_else(|| panic!("{shown:#?}"));
        assert_eq!(shown[at + 1], "    ├ Read a");
        assert_eq!(shown[at + 2], "    └ Edit b");
        // The first run is still collapsed.
        assert!(
            shown
                .iter()
                .any(|s| s.starts_with("  ▸ Read 1  delivery_check 1 "))
        );
        // The state outlives leaving and re-entering the session.
        app.leave_session();
        app.enter_session(AgentRef::Crewmate {
            task: qm_types::ids::TaskId::new(42).unwrap(),
        });
        assert!(app.is_tool_group_expanded(9));
        app.toggle_tool_group(9);
        assert!(!app.is_tool_group_expanded(9));
    }

    #[test]
    fn your_message_sits_in_a_box() {
        let mut app = App::new();
        sample::install_session(&mut app);
        let lines = app.session_view().unwrap().lines.clone();
        let shown = texts(&rows(&lines, &[], 40, &app, true));
        let at = shown
            .iter()
            .position(|s| s.starts_with("╭─ you (direct) "))
            .unwrap();
        assert_eq!(
            shown[at..at + 4],
            [
                "╭─ you (direct) ───────────────────────╮",
                "│ A unit test that reproduces the race │",
                "│ is enough evidence here; skip the    │",
                "│ full e2e run.                        │",
            ]
        );
        assert_eq!(shown[at + 4], format!("╰{}╯", "─".repeat(38)));
        assert!(shown.iter().all(|s| width(s) <= 40));
    }

    #[test]
    fn crewmate_and_secondmate_text_is_drawn_as_markdown() {
        use ratatui::style::Modifier;
        let styled = |rows: &[Row], word: &str, modifier: Modifier| {
            rows.iter()
                .flat_map(|r| &r.line.spans)
                .any(|s| s.content.contains(word) && s.style.add_modifier.contains(modifier))
        };
        // A crewmate's own text.
        let mut app = App::new();
        sample::install_session(&mut app);
        let lines = app.session_view().unwrap().lines.clone();
        let rows = rows(&lines, &[], 100, &app, true);
        let shown = texts(&rows);
        assert!(
            shown.iter().all(|s| !s.contains("**") && !s.contains('`')),
            "{shown:#?}"
        );
        assert!(styled(&rows, "AGENTS.md", Modifier::BOLD), "{shown:#?}");
        assert!(
            styled(&rows, "CONTRIBUTING.md", Modifier::ITALIC),
            "{shown:#?}"
        );
        // A secondmate's replies.
        let mut app = App::new();
        sample::install_secondmate(&mut app);
        let chats = app.session_view().unwrap().conversation.clone();
        let voices = conversation::Voices {
            agent: "S-02",
            supervisor: "supervisor",
        };
        let rows = conversation::rows(&chats, &[], 100, &voices);
        let shown = texts(&rows);
        assert!(shown.iter().any(|s| s.contains("sm/web-app")), "{shown:#?}");
        assert!(shown.iter().all(|s| !s.contains('`')), "{shown:#?}");
    }
}
