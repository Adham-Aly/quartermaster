//! The conversation with the Quartermaster: message headers by source, acknowledgement ticks,
//! queued messages, cards, the busy indicator while it thinks, and scrolling.
//!
//! Content is top-aligned while it fits and pinned to the newest line once it overflows; you
//! scroll up from there (`App::scroll_from_bottom`).

use super::{card, markdown, spinner, stream};
use crate::app::App;
use crate::bridge::{empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane, TextRow};
use crate::text::{cells, truncate, width, wrap_rows};
use crate::theme;
use qm_types::api::ChatLine;
use qm_types::domain::{MessageKind, MessageSource};
use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Selectable text on a drawn row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowText {
    /// The cell, from the row's left, where the text starts.
    pub(crate) offset: u16,
    /// The text exactly as drawn.
    pub(crate) text: String,
    /// The row continues the one before it after a soft wrap.
    pub(crate) continues: bool,
}

/// One drawn row of a scrolling pane: what it shows, its selectable text and its click targets
/// (`(x offset, width, hit)`, relative to the row's left).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub(crate) line: Line<'static>,
    pub(crate) text: Option<RowText>,
    pub(crate) hits: Vec<(u16, u16, Hit)>,
}

impl Row {
    /// A row with nothing to select or click.
    pub(crate) fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            text: None,
            hits: Vec::new(),
        }
    }

    /// A row whose whole content is selectable text.
    pub(crate) fn text(line: Line<'static>, continues: bool) -> Self {
        let text = line.spans.iter().map(|s| s.content.as_ref()).collect();
        Self {
            line,
            text: Some(RowText {
                offset: 0,
                text,
                continues,
            }),
            hits: Vec::new(),
        }
    }
}

/// Draws `rows[start..]` into `area`, as many as fit, and records their selectable text and
/// click targets.
pub(crate) fn draw_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: Vec<Row>,
    start: usize,
    map: &mut FrameMap,
) {
    let mut lines = Vec::new();
    for (y, row) in (area.y..area.bottom()).zip(rows.into_iter().skip(start)) {
        if let Some(text) = row.text
            && !text.text.is_empty()
        {
            map.text.push(TextRow {
                at: Position::new(area.x + text.offset, y),
                text: text.text,
                continues: text.continues,
            });
        }
        for (x, w, hit) in row.hits {
            let x = area.x + x;
            if x < area.right() {
                let w = w.min(area.right() - x);
                map.hits.add(Rect::new(x, y, w, 1), hit);
            }
        }
        lines.push(row.line);
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Draws the conversation and returns how far it can scroll up from its newest line.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) -> usize {
    let block = pane_block("Quartermaster", pane_style(app, Pane::Conversation));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Pane(Pane::Conversation));
    map.scroll_areas.push((area, Pane::Conversation));
    if inner.height == 0 || inner.width == 0 {
        return 0;
    }
    let thinking = app.snapshot.thinking;
    if app.snapshot.conversation.is_empty() && !thinking {
        let lines = empty_lines(
            &[
                "No messages yet.",
                "Type below to talk to the Quartermaster.",
            ],
            inner.width,
        );
        let height = cells(lines.len()).min(inner.height);
        let y = inner.y + (inner.height - height) / 2;
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            Rect::new(inner.x, y, inner.width, height),
        );
        return 0;
    }
    let mut rows = rows(
        &app.snapshot.conversation,
        inner.width,
        app.term.hover,
        &Voices::QUARTERMASTER,
    );
    if thinking {
        rows.extend(thinking_rows(&app.snapshot.conversation, app));
    }
    let visible = usize::from(inner.height);
    let max = rows.len().saturating_sub(visible);
    let offset = app.scroll_from_bottom.min(max);
    let start = rows.len().saturating_sub(visible + offset);
    draw_rows(frame, inner, rows, start, map);
    max
}

/// Who the speakers are in a conversation: the agent it belongs to and the one above it.
pub(crate) struct Voices<'a> {
    /// The header of the agent's own messages: `quartermaster`, or a secondmate's `SM-02`.
    pub(crate) agent: &'a str,
    /// The header of marked messages from the agent's supervisor.
    pub(crate) supervisor: &'a str,
}

impl Voices<'static> {
    /// The Quartermaster's conversation on the Bridge.
    pub(crate) const QUARTERMASTER: Self = Self {
        agent: "quartermaster",
        supervisor: "supervisor",
    };
}

/// Every message's rows at `width`, one blank row between messages, except that the
/// agent's reply to the message right above sits directly under it.
pub(crate) fn rows(
    chats: &[ChatLine],
    width: u16,
    hover: Option<Hit>,
    voices: &Voices<'_>,
) -> Vec<Row> {
    let mut out = Vec::new();
    for (i, chat) in chats.iter().enumerate() {
        if i > 0 && !answers(&chats[i - 1], chat) {
            out.push(Row::plain(Line::default()));
        }
        out.extend(message_rows(chat, width, hover, voices));
    }
    out
}

/// The busy indicator under a conversation whose agent is in a turn and hasn't written yet: right
/// under your message, on the row the reply's header will take; after a blank row otherwise, as
/// any other message would be.
pub(crate) fn thinking_rows(chats: &[ChatLine], app: &App) -> Vec<Row> {
    let answering = chats
        .last()
        .is_none_or(|c| c.source == MessageSource::You && c.card.is_none() && !c.queued);
    let mut out = Vec::new();
    if !answering {
        out.push(Row::plain(Line::default()));
    }
    out.push(Row::plain(spinner::line(app, "thinking")));
    out
}

/// The words in a reply's header that mark it as the answer to the message before it.
const REPLY_CONTEXT: &str = "reply";

/// Whether `chat` is the Quartermaster's reply to `before`, your message with no card under it.
fn answers(before: &ChatLine, chat: &ChatLine) -> bool {
    before.source == MessageSource::You
        && before.card.is_none()
        && chat.source == MessageSource::Agent
        && chat.kind == MessageKind::Reply
        && chat.context.as_deref() == Some(REPLY_CONTEXT)
}

/// Whether the body is Markdown: the Quartermaster's replies and questions.
fn is_markdown(chat: &ChatLine) -> bool {
    chat.source == MessageSource::Agent
        && matches!(chat.kind, MessageKind::Reply | MessageKind::Question)
}

/// One message: its header, its body, and its card. Your own messages sit in a box.
fn message_rows(chat: &ChatLine, max: u16, hover: Option<Hit>, voices: &Voices<'_>) -> Vec<Row> {
    if chat.source == MessageSource::You
        && let Some(mut out) = prompt_box(title(chat, voices), right(chat), &chat.text, max)
    {
        if let Some(card) = &chat.card {
            out.extend(card::rows(card, chat.id, max, hover));
        }
        return out;
    }
    let mut out = vec![Row::text(header(chat, max, voices), false)];
    let body_style = if chat.source == MessageSource::App {
        theme::dim()
    } else {
        theme::text()
    };
    let committed = stream::committed(&chat.text, chat.streaming);
    if chat.streaming && committed.is_empty() {
        out.push(Row::plain(Line::styled("…", theme::dim())));
    } else {
        let body = committed.strip_suffix('\n').unwrap_or(committed);
        if is_markdown(chat) {
            for (line, continues) in markdown::rows(body, usize::from(max)) {
                out.push(Row::text(line, continues));
            }
        } else {
            for (row, continues) in wrap_rows(body, usize::from(max)) {
                out.push(Row::text(Line::styled(row, body_style), continues));
            }
        }
    }
    if let Some(card) = &chat.card {
        out.extend(card::rows(card, chat.id, max, hover));
    }
    out
}

/// `you 13:41 · context` on the left, `✓ acknowledgement` or `queued` at the right edge.
fn header(chat: &ChatLine, max: u16, voices: &Voices<'_>) -> Line<'static> {
    let mut spans = title(chat, voices);
    if let Some(right) = right(chat) {
        let used: usize = spans.iter().map(|s| width(&s.content)).sum();
        let pad = usize::from(max).saturating_sub(used + width(&right.content));
        // Dropped when it doesn't fit with one space to spare.
        if pad >= 1 {
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(right);
        }
    }
    Line::from(spans)
}

/// `you 13:41 · context`. Names are bold in the text colour; a supervisor's marked line keeps
/// its violet, the app's words their dim.
fn title(chat: &ChatLine, voices: &Voices<'_>) -> Vec<Span<'static>> {
    let (name, name_style) = match chat.source {
        MessageSource::You => ("you", theme::text()),
        MessageSource::Agent => (voices.agent, theme::text()),
        MessageSource::Supervisor if voices.supervisor == "supervisor" => {
            (voices.supervisor, theme::violet())
        }
        MessageSource::Supervisor => (voices.supervisor, theme::text()),
        MessageSource::App => ("app", theme::dim()),
    };
    let time = chat
        .at
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M")
        .to_string();
    let mut spans = vec![
        Span::styled(name.to_owned(), name_style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(time, theme::dim()),
    ];
    if let Some(context) = &chat.context {
        spans.push(Span::styled(format!(" · {context}"), theme::dim()));
    }
    spans
}

/// `✓ acknowledgement` in the ok colour, or a dim `queued`.
fn right(chat: &ChatLine) -> Option<Span<'static>> {
    match &chat.acknowledgement {
        Some(ack) => Some(Span::styled(format!("✓ {ack}"), theme::ok())),
        None if chat.queued => Some(Span::styled("queued", theme::dim())),
        None => None,
    }
}

/// The narrowest pane a prompt box is drawn in: below it, your message is drawn unboxed.
const BOX_MIN: u16 = 12;

/// Your prompt in a dim rounded box `max` cells wide, so it stands apart from the agent's text:
/// `title` in the top border, `right` at its right end (dropped when it doesn't fit), the text
/// wrapped one cell inside. `None` when the pane is too narrow for a box.
///
/// ```text
/// ╭─ you 14:12 ─────────────────────────── ✓ Steered: T-0042 ─╮
/// │ tell 42 a unit test that reproduces the race is enough    │
/// ╰───────────────────────────────────────────────────────────╯
/// ```
pub(crate) fn prompt_box(
    title: Vec<Span<'static>>,
    right: Option<Span<'static>>,
    text: &str,
    max: u16,
) -> Option<Vec<Row>> {
    if max < BOX_MIN {
        return None;
    }
    let border = theme::dim();
    let total = usize::from(max);
    // `╭─ ` and the space and `╮` after the title.
    let room = total - 5;
    let mut title = fit_spans(title, room);
    let title_text: String = title.iter().map(|s| s.content.as_ref()).collect();
    let mut used = width(&title_text);
    let mut top = vec![Span::styled("╭─ ", border)];
    top.append(&mut title);
    top.push(Span::styled(" ", border));
    used += 4;
    // ` ✓ Steered ─╮` after the fill, with at least one `─` before it.
    let right = right.filter(|r| used + 1 + width(&r.content) + 3 <= total);
    let right_width = right.as_ref().map_or(0, |r| width(&r.content) + 3);
    top.push(Span::styled(
        "─".repeat(total - used - right_width - 1),
        border,
    ));
    if let Some(right) = right {
        top.push(Span::styled(" ", border));
        top.push(right);
        top.push(Span::styled(" ─", border));
    }
    top.push(Span::styled("╮", border));
    let mut out = vec![Row {
        line: Line::from(top),
        text: Some(RowText {
            offset: 3,
            text: title_text,
            continues: false,
        }),
        hits: Vec::new(),
    }];
    let inner = total - 4;
    let body = text.strip_suffix('\n').unwrap_or(text);
    if !body.is_empty() {
        for (row, continues) in wrap_rows(body, inner) {
            let pad = inner.saturating_sub(width(&row));
            out.push(Row {
                line: Line::from(vec![
                    Span::styled("│ ", border),
                    Span::styled(row.clone(), theme::text()),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(" │", border),
                ]),
                text: Some(RowText {
                    offset: 2,
                    text: row,
                    continues,
                }),
                hits: Vec::new(),
            });
        }
    }
    out.push(Row::plain(Line::styled(
        format!("╰{}╯", "─".repeat(total - 2)),
        border,
    )));
    Some(out)
}

/// Spans cut to `max` cells in all, the last one shown ending in `…`.
pub(crate) fn fit_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_prompt_box_fills_the_width_and_drops_what_does_not_fit() {
        let title = || vec![Span::raw("you 14:12")];
        let ack = || Some(Span::raw("✓ Steered: T-0042"));
        let rows = prompt_box(title(), ack(), "tell 42 to skip the e2e run", 40).unwrap();
        assert_eq!(
            texts(&rows),
            [
                "╭─ you 14:12 ────── ✓ Steered: T-0042 ─╮",
                "│ tell 42 to skip the e2e run          │",
                "╰──────────────────────────────────────╯",
            ]
        );
        assert_eq!(rows[0].text.as_ref().unwrap().text, "you 14:12");
        assert_eq!(rows[1].text.as_ref().unwrap().offset, 2);
        // Too narrow for the acknowledgement: it is dropped, the box still closes.
        let rows = prompt_box(title(), ack(), "hi", 24).unwrap();
        assert_eq!(texts(&rows)[0], format!("╭─ you 14:12 {}╮", "─".repeat(10)));
        assert!(texts(&rows).iter().all(|s| width(s) == 24));
        // Too narrow for a box at all.
        assert!(prompt_box(title(), None, "hi", BOX_MIN - 1).is_none());
    }
}
