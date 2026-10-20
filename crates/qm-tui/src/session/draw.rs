//! Drawing a worker's session:
//!
//! ```text
//!  T-0042 · fix(auth): token refresh race · api-server · sonnet-5-5 · supervisor: Quartermaster  ← Back
//!  Talking to this crewmate directly. You override its supervisor; it will reconcile.   (warn)
//! ╭─ Session ──────────────────────────────────────────────────────────────────────────────╮
//! │ [from supervisor] Brief · T-0042 · fix(auth): token refresh race  Expand                │
//! │                                                                                         │
//! │ Reading AGENTS.md and CONTRIBUTING.md, then calling delivery_check before any edit.     │
//! │   ▸ delivery_check  → push ok · PR ok · checks readable                    (dim)        │
//! │                                                                                         │
//! │ you (direct)                                                                            │
//! │ A unit test that reproduces the race is enough evidence here; skip the full e2e run.    │
//! ╰─────────────────────────────────────────────────────────────────────────────────────────╯
//! ╭─────────────────────────────────────────────────────────────────────────────────────────╮
//! │ › message T-0042 directly…                                                              │
//! ╰─────────────────────────────────────────────────────────────────────────────────────────╯
//!  running 14m · job J3 idle                                       Interrupt  · esc back · ⏎ send
//! ```
//!
//! No top bar and no Bridge status bar: the status line carries the Bridge's `⚑ 1 approval` buttons
//! first while something waits for you; a click goes back to the card.
//!
//! A secondmate's session has the same frame: the header reads `SM-02 · secondmate · {scope} ·
//! {status}`, its charter's first line shows dim in the banner's row, and the box holds its
//! conversation, drawn by the Bridge's chat widgets.

use super::{BANNER, SESSION_SHOWS_ATTENTION, SessionHit};
use crate::app::App;
use crate::bridge::pane_block;
use crate::chrome::{MIN_HEIGHT, MIN_WIDTH};
use crate::hit::{FrameMap, Hit, Pane, StatusItem};
use crate::text::{cells, truncate, width, wrap, wrap_rows, wrap_styled};
use crate::theme;
use crate::widgets::conversation::{self, Row, RowText, draw_rows};
use crate::widgets::{button, input_box, markdown, stream};
use qm_types::api::{AgentRef, SessionLine, SessionLineKind, SessionView};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The `← Back` button's label.
const BACK: &str = "← Back";
/// The status line's `Interrupt` button.
const INTERRUPT: &str = "Interrupt";
/// The hints after it.
const HINTS: &str = "esc back · ⏎ send";

/// Draws the whole screen for `agent`'s session into `area`.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent: AgentRef,
    map: &mut FrameMap,
) {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }
    let view = app.session_view();
    header(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        app,
        agent,
        view,
        map,
    );
    let subtitle = view.map(|v| v.subtitle.as_str()).filter(|s| !s.is_empty());
    banner(
        frame,
        Rect::new(area.x, area.y + 1, area.width, 1),
        subtitle,
    );
    let status_row = area.bottom() - 1;
    let top = area.y + 2;
    let input_height = input_box::height(app, area.width).min((status_row - top) / 2);
    let input_area = Rect::new(area.x, status_row - input_height, area.width, input_height);
    let session_area = Rect::new(area.x, top, area.width, input_area.y - top);
    session_pane(frame, session_area, app, view, map);
    input(frame, input_area, app, agent, view, map);
    status_line(
        frame,
        Rect::new(area.x, status_row, area.width, 1),
        app,
        view,
        map,
    );
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let message = format!(
        "Quartermaster needs a terminal of at least {MIN_WIDTH}×{MIN_HEIGHT}; this one is {}×{}.",
        area.width, area.height
    );
    let rows = wrap(&message, usize::from(area.width.saturating_sub(2)));
    let height = cells(rows.len()).min(area.height);
    let y = area.y + (area.height - height) / 2;
    let lines: Vec<Line<'_>> = rows.into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(theme::dim()),
        Rect::new(area.x, y, area.width, height),
    );
}

/// How the agent is named: `T-0042`, `SM-02`.
pub(crate) fn agent_label(agent: AgentRef) -> String {
    match agent {
        AgentRef::Crewmate { task } => task.to_string(),
        AgentRef::Secondmate { id } => id.to_string(),
        AgentRef::Quartermaster => "Quartermaster".to_owned(),
    }
}

/// `T-0042` bold, ` · ` dim, the title, ` · ` dim, the note dim; `← Back` at the right.
fn header(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    agent: AgentRef,
    view: Option<&SessionView>,
    map: &mut FrameMap,
) {
    let back_width = button::width_of(BACK);
    let back_x = area.right().saturating_sub(back_width + 1);
    let hit = Hit::Session(SessionHit::Back);
    frame.render_widget(
        Line::from(button::span(BACK, app.hover == Some(hit))),
        Rect::new(back_x, area.y, back_width, 1),
    );
    map.hits.add(Rect::new(back_x, area.y, back_width, 1), hit);

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(agent_label(agent), theme::strong()),
    ];
    let title = view
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
    if let Some(title) = title {
        spans.push(Span::styled(" · ", theme::dim()));
        spans.push(Span::styled(title, theme::text()));
    }
    if let Some(note) = view.map(|v| v.note.as_str()).filter(|n| !n.is_empty()) {
        spans.push(Span::styled(" · ", theme::dim()));
        spans.push(Span::styled(note.to_owned(), theme::dim()));
    }
    let room = usize::from(back_x.saturating_sub(area.x + 2));
    frame.render_widget(
        Line::from(fit(spans, room)),
        Rect::new(area.x, area.y, back_x.saturating_sub(area.x), 1),
    );
}

/// Spans cut to `max` cells in all, the last one shown ending in `…`.
fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = width(&span.content);
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let left = max.saturating_sub(used);
        if left > 0 {
            out.push(Span::styled(truncate(&span.content, left), span.style));
        }
        break;
    }
    out
}

/// The banner in the warn colour on the terminal's background (on the 16-colour bright-black fill
/// it can't be read in a light theme); for a secondmate, its charter's first line, dim.
fn banner(frame: &mut Frame<'_>, area: Rect, subtitle: Option<&str>) {
    if let Some(subtitle) = subtitle {
        let text = truncate(&format!(" {subtitle}"), usize::from(area.width));
        frame.render_widget(Paragraph::new(Line::styled(text, theme::dim())), area);
        return;
    }
    let text = truncate(&format!(" {BANNER}"), usize::from(area.width));
    frame.render_widget(Paragraph::new(Line::styled(text, theme::warn())), area);
}

/// The session's box: its lines, pinned to the newest once they overflow.
fn session_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    view: Option<&SessionView>,
    map: &mut FrameMap,
) {
    let focused = app.focus == Pane::Session;
    let style = theme::border(focused, app.hover == Some(Hit::Pane(Pane::Session)));
    let block = pane_block("Session", style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Pane(Pane::Session));
    map.scroll_areas.push((area, Pane::Session));
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let lines = view.map_or(&[][..], |v| v.lines.as_slice());
    let chats = view.map_or(&[][..], |v| v.conversation.as_slice());
    if lines.is_empty() && chats.is_empty() {
        let text = if view.is_some() {
            "Nothing in this session yet."
        } else {
            "Opening the session…"
        };
        let y = inner.y + inner.height / 2;
        frame.render_widget(
            Paragraph::new(Line::styled(text, theme::dim())).alignment(Alignment::Center),
            Rect::new(inner.x, y, inner.width, 1),
        );
        return;
    }
    let rows = if chats.is_empty() {
        rows(lines, inner.width, app)
    } else {
        // A secondmate's conversation, from its side: its own replies under its name, the
        // Quartermaster's messages under the Quartermaster's.
        let name = view.map_or_else(String::new, |v| agent_label(v.agent));
        let voices = conversation::Voices {
            agent: &name,
            supervisor: "quartermaster",
        };
        conversation::rows(chats, inner.width, app.hover, &voices)
    };
    let visible = usize::from(inner.height);
    let max = rows.len().saturating_sub(visible);
    let offset = app.session_scroll().min(max);
    let start = rows.len().saturating_sub(visible + offset);
    map.scroll_max.push((Pane::Session, max));
    draw_rows(frame, inner, rows, start, map);
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
/// message.
pub(crate) fn rows(lines: &[SessionLine], max: u16, app: &App) -> Vec<Row> {
    let mut out = Rows { rows: Vec::new() };
    for line in lines {
        match line.kind {
            SessionLineKind::Supervisor | SessionLineKind::App => {
                marked(&mut out, line, max, app);
                out.blank();
            }
            SessionLineKind::Tool => {
                out.wrapped(&line.text, max, 4, "  ▸ ", theme::dim());
            }
            SessionLineKind::You => {
                out.blank();
                out.rows.push(Row::text(
                    Line::from(vec![
                        Span::styled("you", theme::strong()),
                        Span::styled(" (direct)", theme::dim()),
                    ]),
                    false,
                ));
                out.wrapped(&line.text, max, 0, "", theme::text());
                out.blank();
            }
            // The worker's text, and any kind a later build adds.
            _ => {
                let committed = stream::committed(&line.text, line.streaming);
                if line.streaming && committed.is_empty() {
                    out.rows.push(Row::plain(Line::styled("…", theme::dim())));
                } else {
                    let body = committed.strip_suffix('\n').unwrap_or(committed);
                    out.wrapped(body, max, 0, "", theme::text());
                }
            }
        }
    }
    out.rows
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
                spans.push(button::span(button.0, app.hover == Some(button.1)));
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
                    line: Line::from(button::span(button.0, app.hover == Some(button.1))),
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

/// `⚑ 1 approval · running 14m · job J3 idle` on the left; `Interrupt · esc back · ⏎ send` on
/// the right.
fn status_line(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    view: Option<&SessionView>,
    map: &mut FrameMap,
) {
    // The right side first: Interrupt while a turn runs, then the hints.
    let running = view.is_some_and(|v| v.turn_running);
    let hints = if running {
        format!(" · {HINTS}")
    } else {
        HINTS.to_owned()
    };
    let button_width = if running {
        button::width_of(INTERRUPT)
    } else {
        0
    };
    let right_width = button_width + cells(width(&hints));
    let right_x = area.right().saturating_sub(right_width + 1);

    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    let notice = app
        .quit_notice()
        .map(str::to_owned)
        .or_else(|| app.view.flash.as_ref().map(|f| f.text.clone()));
    if let Some(notice) = notice {
        left.push(Span::styled(notice, theme::accent()));
    } else {
        let attention = app.snapshot.attention;
        if SESSION_SHOWS_ATTENTION && attention.approvals + attention.decisions > 0 {
            // The Bridge status bar's buttons, one space apart: a click goes back to the Bridge
            // and brings that card into view.
            let mut x = area.x + 1;
            for (n, one, many, item) in [
                (
                    attention.approvals,
                    "approval",
                    "approvals",
                    StatusItem::Approvals,
                ),
                (
                    attention.decisions,
                    "decision",
                    "decisions",
                    StatusItem::Decisions,
                ),
            ] {
                if n == 0 {
                    continue;
                }
                if x > area.x + 1 {
                    left.push(Span::raw(" "));
                    x += 1;
                }
                let label = counted(n, one, many);
                let hit = Hit::Status(item);
                let w = button::width_of(&label);
                left.push(button::span(&label, app.hover == Some(hit)));
                if x + w < right_x {
                    map.hits.add(Rect::new(x, area.y, w, 1), hit);
                }
                x += w;
            }
            left.push(Span::styled(" · ", theme::dim()));
        }
        if let Some(status) = view.map(|v| v.status.as_str()).filter(|s| !s.is_empty()) {
            left.push(Span::styled(status.to_owned(), theme::dim()));
        }
    }
    let room = usize::from(right_x.saturating_sub(area.x + 2));
    let left_line = Line::from(fit(left, room));
    let left_width = cells(left_line.width());
    frame.render_widget(left_line, area);
    if area.x + left_width + 2 > right_x {
        return;
    }
    let mut spans = Vec::new();
    if running {
        let hit = Hit::Session(SessionHit::Interrupt);
        spans.push(button::span(INTERRUPT, app.hover == Some(hit)));
        map.hits
            .add(Rect::new(right_x, area.y, button_width, 1), hit);
    }
    spans.push(Span::styled(hints, theme::dim()));
    frame.render_widget(
        Line::from(spans),
        Rect::new(right_x, area.y, right_width, 1),
    );
}

fn counted(n: u32, one: &str, many: &str) -> String {
    format!("⚑ {n} {}", if n == 1 { one } else { many })
}
