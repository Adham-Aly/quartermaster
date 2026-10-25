//! Approval, decision, notice and question cards under a message.
//!
//! A card is a rounded box the full inner width of the conversation, drawn as rows of the
//! conversation itself so it scrolls with the messages around it.

use super::button;
use super::conversation::{Row, RowText};
use crate::hit::Hit;
use crate::text::{cells, truncate, width, wrap_styled};
use crate::theme;
use qm_types::api::{Card, CardKind};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The border and padding on each side of a card's text: `│ ` and ` │`.
const FRAME: u16 = 2;

/// Spaces before the first button of a row.
const BUTTONS_INDENT: u16 = 2;

/// A button as one way of showing a card draws it: its label, its hit and whether it is lit
/// (hovered, or highlighted from the keyboard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Button {
    pub(crate) label: String,
    pub(crate) hit: Hit,
    pub(crate) lit: bool,
}

/// The card's rows with `buttons` in place of its own (the Actions overlay adds "Go to" and
/// key letters); `dimmed` draws them dim, after you pressed one.
pub(crate) fn rows_with(
    card: &Card,
    width: u16,
    buttons: &[Button],
    hover: Option<Hit>,
    dimmed: bool,
) -> Vec<Row> {
    let colour = match card.kind {
        CardKind::Notice => theme::warn(),
        _ => theme::accent(),
    };
    let inner = usize::from(width.saturating_sub(2 * FRAME)).max(1);
    let mut out = vec![edge('╭', '╮', width, colour)];

    // Line 1: the glyph and heading, then the title and the note.
    let glyph = match card.kind {
        CardKind::Notice => "⚠",
        CardKind::Question => "?",
        CardKind::Plan => "☰",
        CardKind::Review => "✎",
        CardKind::Report => "✓",
        _ => "⚑",
    };
    let mut heading = vec![(format!("{glyph} {}", card.heading), colour)];
    if let Some(title) = &card.title {
        heading.push((format!(" · {title}"), theme::text()));
    }
    if let Some(note) = &card.note {
        heading.push((format!(" {note}"), theme::dim()));
    }
    push_wrapped(&mut out, &heading, 0, inner, colour);

    // The card's lines, each wrapped with every row indented.
    for line in &card.lines {
        let runs: Vec<(String, Style)> = line
            .spans
            .iter()
            .map(|s| (s.text.clone(), theme::tone(s.tone)))
            .collect();
        let indent = usize::from(line.indent).min(inner.saturating_sub(1));
        push_wrapped(&mut out, &runs, indent, inner, colour);
    }

    // The buttons, or what you answered.
    if let Some(answered) = &card.answered {
        let text = truncate(&format!("✓ {answered}"), inner.saturating_sub(2));
        let content = vec![
            Span::raw(" ".repeat(usize::from(BUTTONS_INDENT))),
            Span::styled(text, theme::dim()),
        ];
        out.push(boxed(content, inner, colour, None, Vec::new()));
    }
    if (card.answered.is_none() || buttons.len() > card.buttons.len()) && !buttons.is_empty() {
        let gap: u16 = if card.kind == CardKind::Approval {
            1
        } else {
            2
        };
        let mut content: Vec<Span<'static>> = Vec::new();
        let mut hits: Vec<(u16, u16, Hit)> = Vec::new();
        let mut x = 0u16;
        let shown = if card.answered.is_some() {
            &buttons[card.buttons.len()..]
        } else {
            buttons
        };
        for b in shown {
            let w = button::width_of(&b.label);
            let lead = if x == 0 { BUTTONS_INDENT } else { gap };
            if x > 0 && usize::from(x + lead + w) > inner {
                let row = std::mem::take(&mut content);
                out.push(boxed(row, inner, colour, None, std::mem::take(&mut hits)));
                x = 0;
            }
            let lead = if x == 0 { BUTTONS_INDENT } else { gap };
            content.push(Span::raw(" ".repeat(usize::from(lead))));
            x += lead;
            let hit = b.hit;
            if dimmed {
                content.push(Span::styled(format!(" {} ", b.label), theme::dim()));
            } else {
                content.push(button::span(&b.label, b.lit || hover == Some(hit)));
                hits.push((FRAME + x, w, hit));
            }
            x += w;
        }
        out.push(boxed(content, inner, colour, None, hits));
    }

    out.push(edge('╰', '╯', width, colour));
    out
}

/// A top or bottom border: `╭───╮`.
fn edge(left: char, right: char, width: u16, colour: Style) -> Row {
    let middle = "─".repeat(usize::from(width.saturating_sub(2)));
    Row::plain(Line::styled(format!("{left}{middle}{right}"), colour))
}

/// Wraps `runs` into the card, `indent` cells in, as selectable rows.
fn push_wrapped(
    out: &mut Vec<Row>,
    runs: &[(String, Style)],
    indent: usize,
    inner: usize,
    colour: Style,
) {
    for (i, row) in wrap_styled(runs, inner - indent).into_iter().enumerate() {
        let text: String = row.iter().map(|(t, _)| t.as_str()).collect();
        let mut content = vec![Span::raw(" ".repeat(indent))];
        content.extend(row.into_iter().map(|(t, s)| Span::styled(t, s)));
        let selectable = RowText {
            offset: FRAME + cells(indent),
            text,
            continues: i > 0,
        };
        out.push(boxed(content, inner, colour, Some(selectable), Vec::new()));
    }
}

/// One row inside the card's border: `│ ` + content padded to `inner` + ` │`.
fn boxed(
    content: Vec<Span<'static>>,
    inner: usize,
    colour: Style,
    text: Option<RowText>,
    hits: Vec<(u16, u16, Hit)>,
) -> Row {
    let used: usize = content.iter().map(|s| width(&s.content)).sum();
    let mut spans = vec![Span::styled("│ ", colour)];
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
    spans.push(Span::styled(" │", colour));
    Row {
        line: Line::from(spans),
        text,
        hits,
    }
}
