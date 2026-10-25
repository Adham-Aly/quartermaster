//! The conversation with the Quartermaster: message headers by source, acknowledgement ticks,
//! queued messages, cards, the busy indicator while it thinks, and scrolling.
//!
//! Content is top-aligned while it fits and pinned to the newest line once it overflows; you
//! scroll up from there (`App::scroll_from_bottom`).

use super::{context_strip, markdown, spinner, stream};
use crate::app::App;
use crate::bridge::{empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane, TextRow};
use crate::logo;
use crate::text::{cells, truncate, width, wrap_rows};
use crate::theme;
use qm_types::api::{AgentRef, ChatLine, CompactionNote};
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
        // The logo above the words, a row apart, where the terminal shows images and there is
        // room for it with a margin all round.
        let hero = app.term.logo.map(|ink| {
            let rows = logo::HERO_ROWS;
            Rect::new(0, 0, ink.columns(rows), rows)
        });
        let hero = hero
            .filter(|h| h.width + 4 <= inner.width && h.height + 1 + height + 4 <= inner.height);
        let total = hero.map_or(height, |h| h.height + 1 + height);
        let top = inner.y + (inner.height - total) / 2;
        let y = top + hero.map_or(0, |h| h.height + 1);
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            Rect::new(inner.x, y, inner.width, height),
        );
        if let Some(hero) = hero {
            let area = Rect {
                x: inner.x + (inner.width - hero.width) / 2,
                y: top,
                ..hero
            };
            map.logos
                .push(logo::reserve(frame.buffer_mut(), logo::Size::Hero, area));
        }
        return 0;
    }
    let agent = AgentRef::Quartermaster;
    let mut rows = rows(
        &app.snapshot.conversation,
        &context_strip::chat_notes(app, agent),
        inner.width,
        &Voices::QUARTERMASTER,
    );
    if let Some(row) = context_strip::compacting_row(app, agent) {
        rows.push(Row::plain(Line::default()));
        rows.push(row);
    } else if thinking {
        rows.extend(thinking_rows(&app.snapshot.conversation, app, agent));
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
    /// The header of the agent's own messages: `quartermaster`, or a secondmate's `S-02`.
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
/// agent's reply to the message right above sits directly under it, and one-line notes in a run
/// stack.
/// Compaction notes sit between the messages by time.
pub(crate) fn rows(
    chats: &[ChatLine],
    notes: &[&CompactionNote],
    width: u16,
    voices: &Voices<'_>,
) -> Vec<Row> {
    let mut notes = notes;
    let mut out = Vec::new();
    for (i, chat) in chats.iter().enumerate() {
        let noted = context_strip::notes_before(&mut notes, Some(chat.at), width, out.is_empty());
        let after_note = !noted.is_empty();
        out.extend(noted);
        let stacked = i > 0 && chat.one_line && chats[i - 1].one_line;
        if after_note || (i > 0 && !answers(&chats[i - 1], chat) && !stacked) {
            out.push(Row::plain(Line::default()));
        }
        out.extend(message_rows(chat, width, voices));
    }
    let first = out.is_empty();
    out.extend(context_strip::notes_before(&mut notes, None, width, first));
    out
}

/// The busy indicator under a conversation whose agent is in a turn and hasn't written yet: right
/// under your message, on the row the reply's header will take; after a blank row otherwise, as
/// any other message would be.
pub(crate) fn thinking_rows(chats: &[ChatLine], app: &App, agent: AgentRef) -> Vec<Row> {
    let spinner = spinner::Spinner::of(agent);
    let answering = chats
        .last()
        .is_none_or(|c| c.source == MessageSource::You && !c.queued);
    let mut out = Vec::new();
    if !answering {
        out.push(Row::plain(Line::default()));
    }
    out.push(Row::plain(spinner::line(
        app,
        spinner,
        &spinner::verb(spinner, chats.last().map(|c| c.at)),
    )));
    out
}

/// The words in a reply's header that mark it as the answer to the message before it.
const REPLY_CONTEXT: &str = "reply";

/// Whether `chat` is the Quartermaster's reply to `before`, your message.
fn answers(before: &ChatLine, chat: &ChatLine) -> bool {
    before.source == MessageSource::You
        && chat.source == MessageSource::Agent
        && chat.kind == MessageKind::Reply
        && chat.context.as_deref() == Some(REPLY_CONTEXT)
}

/// Whether the body is Markdown: the Quartermaster's replies and questions.
fn is_markdown(chat: &ChatLine) -> bool {
    chat.source == MessageSource::Agent
        && matches!(chat.kind, MessageKind::Reply | MessageKind::Question)
}

/// One message: its header and its body; never a card, even if the line carries one (cards show
/// only in the Actions overlay). Your own messages sit in a box; a one-line note is a single row.
fn message_rows(chat: &ChatLine, max: u16, voices: &Voices<'_>) -> Vec<Row> {
    if chat.one_line {
        return vec![one_line_row(chat, max)];
    }
    if chat.source == MessageSource::You
        && let Some(out) = prompt_box(title(chat, voices), right(chat), &chat.text, max)
    {
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
    out
}

/// A one-line note: a dim `✓ text`, cut with `…` to fit, and its time at the right edge while
/// there is room for it with a space to spare.
///
/// ```text
/// ✓ Plan reply sent from the page · T-0324 round 2 · 3 of 3 answered            14:12
/// ```
fn one_line_row(chat: &ChatLine, max: u16) -> Row {
    let total = usize::from(max);
    let time = chat
        .at
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M")
        .to_string();
    let text = format!("✓ {}", chat.text.lines().next().unwrap_or_default().trim());
    // The time keeps a space before it and leaves the text at least a dozen cells.
    let keeps_time = total >= width(&time) + 1 + 12;
    let room = if keeps_time {
        total - width(&time) - 1
    } else {
        total
    };
    let shown = truncate(&text, room);
    let mut spans = vec![Span::styled(shown.clone(), theme::dim())];
    if keeps_time {
        let pad = total - width(&shown) - width(&time);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(time, theme::dim()));
    }
    Row {
        line: Line::from(spans),
        text: Some(RowText {
            offset: 0,
            text: shown,
            continues: false,
        }),
        hits: Vec::new(),
    }
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
/// ╭─ you 14:12 ─────────────────────────── ✓ Steered: C-0042 ─╮
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
        let ack = || Some(Span::raw("✓ Steered: C-0042"));
        let rows = prompt_box(title(), ack(), "tell 42 to skip the e2e run", 40).unwrap();
        assert_eq!(
            texts(&rows),
            [
                "╭─ you 14:12 ────── ✓ Steered: C-0042 ─╮",
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

    #[test]
    fn a_page_sent_plan_reply_is_one_dim_row_and_notes_in_a_run_stack() {
        use qm_types::ids::MessageId;
        let at = jiff::Timestamp::from_second(1_800_000_000).unwrap();
        let time = at
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M")
            .to_string();
        let line = |id: i64, source, text: &str| {
            ChatLine::new(MessageId::new(id), source, MessageKind::Message, at, text)
        };
        let note = |id: i64, text: &str| line(id, MessageSource::App, text).with_one_line();
        let sent = "Plan reply sent from the page · T-0324 round 2 · 3 of 3 answered";
        let chats = [
            line(1, MessageSource::You, "plan it"),
            note(2, sent),
            note(
                3,
                "Plan reply sent from the page · C-0051 round 1 · 1 of 2 answered",
            ),
            line(
                4,
                MessageSource::App,
                "C-0051's plan, round 2, has 1 question for you",
            ),
        ];
        let voices = Voices::QUARTERMASTER;
        let drawn = texts(&rows(&chats, &[], 80, &voices));
        let at_note = drawn.iter().position(|r| r.starts_with('✓')).unwrap();
        // A blank row before the run, none inside it, one after it; no header for either note.
        assert_eq!(drawn[at_note - 1], "");
        assert!(drawn[at_note + 1].starts_with("✓ Plan reply sent from the page · C-0051"));
        assert_eq!(drawn[at_note + 2], "");
        assert!(drawn[at_note + 3].starts_with("app "));
        let first = &drawn[at_note];
        assert_eq!(width(first), 80);
        assert!(first.starts_with(&format!("✓ {sent}")));
        assert!(first.ends_with(&format!(" {time}")));

        let row = one_line_row(&chats[1], 40);
        let text = texts(std::slice::from_ref(&row)).remove(0);
        assert_eq!(width(&text), 40);
        assert!(text.ends_with(&format!("… {time}")), "{text}");
        assert!(
            row.line
                .spans
                .iter()
                .all(|s| s.content.trim().is_empty() || s.style == theme::dim())
        );
        // Its selectable text is the note, without the padding or the time.
        assert!(row.text.as_ref().unwrap().text.starts_with("✓ Plan reply"));
        assert!(!row.text.as_ref().unwrap().text.contains(&time));
        // Too narrow to keep the time: the note alone, cut to the width.
        let text = texts(&[one_line_row(&chats[1], 16)]).remove(0);
        assert_eq!(width(&text), 16);
        assert!(text.ends_with('…') && !text.contains(&time), "{text}");
    }
}
