//! The Actions inbox in the left column: one row per request waiting on you, newest first after
//! the app's own configuration prompts. A row only selects; buttons are pressed in the overlay.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, pane_block, pane_style};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width};
use crate::theme;
use qm_types::api::{ActionItem, ActionKind, AgentRef, CardKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The empty inbox's one row.
pub(crate) const EMPTY: &str = "Nothing needs you.";

/// Draws the Actions box into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    if area.height == 0 {
        return;
    }
    let title = match app.snapshot.actions.len() {
        0 => "Actions".to_owned(),
        n => format!("Actions · {n}"),
    };
    let block = pane_block(&title, pane_style(app, Pane::Actions));
    let rows = rows(app, block.inner(area).width);
    draw_list(
        frame,
        area,
        block,
        rows,
        app.view.actions_scroll,
        Pane::Actions,
        map,
    );
}

/// The pane's rows at `inner_width`.
pub(crate) fn rows(app: &App, inner_width: u16) -> Vec<ListRow> {
    let items = &app.snapshot.actions;
    if items.is_empty() {
        return empty_lines(&[EMPTY], inner_width)
            .into_iter()
            .map(ListRow::plain)
            .collect();
    }
    let now = jiff::Timestamp::now();
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let hit = Hit::ActionRow(i);
            let selected = (app.view.selected_action.as_ref() == Some(&item.key))
                .then_some(!app.view.pointer_picked);
            let hovered = selected != Some(true) && app.term.hover == Some(hit);
            ListRow {
                line: line(item, usize::from(inner_width), now, hovered),
                hit: Some(hit),
                selected,
            }
        })
        .collect()
}

/// The glyph and its colour for an item.
pub(crate) fn glyph(item: &ActionItem) -> (&'static str, Style) {
    match item.kind {
        ActionKind::Approval => ("⚑", theme::accent()),
        ActionKind::Decision => ("◆", theme::warn()),
        ActionKind::Plan
            if item
                .card
                .buttons
                .iter()
                .any(|b| matches!(b.action, qm_types::api::CardAction::ApprovePlan { .. })) =>
        {
            ("☰", theme::accent())
        }
        ActionKind::Plan => ("☰", theme::warn()),
        ActionKind::Question => ("?", theme::warn()),
        ActionKind::Notice => ("!", theme::warn()),
        _ => match item.card.kind {
            CardKind::Approval => ("⚑", theme::accent()),
            _ => ("⚙", theme::warn()),
        },
    }
}

/// Who asked, as a short label: the task, `QM`, `SM-n`, or nothing for the app's own prompts.
pub(crate) fn source(item: &ActionItem) -> Option<String> {
    if let Some(task) = item.task {
        return Some(task.to_string());
    }
    match item.from? {
        AgentRef::Quartermaster => Some("QM".to_owned()),
        AgentRef::Secondmate { id } => Some(id.to_string()),
        AgentRef::Crewmate { task } => Some(task.to_string()),
    }
}

/// How long ago, in one short word: `now`, `3m`, `2h`, `4d`.
pub(crate) fn age(at: jiff::Timestamp, now: jiff::Timestamp) -> String {
    let secs = now.as_second().saturating_sub(at.as_second()).max(0);
    match secs {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn line(item: &ActionItem, inner: usize, now: jiff::Timestamp, hovered: bool) -> Line<'static> {
    let (glyph, style) = glyph(item);
    let age = age(item.at, now);
    let text_style = if hovered {
        theme::strong()
    } else {
        theme::text()
    };
    let mut words = String::new();
    if let Some(source) = source(item)
        && !item.title.starts_with(&source)
    {
        words.push_str(&source);
        words.push(' ');
    }
    words.push_str(&item.title);
    // Glyph, space, words, at least one space, the age.
    let room = inner.saturating_sub(2 + 1 + width(&age));
    let words = truncate(&words, room);
    let gap = inner.saturating_sub(2 + width(&words) + width(&age)).max(1);
    Line::from(vec![
        Span::styled(glyph, style),
        Span::raw(" "),
        Span::styled(words, text_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(age, theme::dim()),
    ])
}

#[cfg(test)]
mod tests {
    use super::age;
    use jiff::Timestamp;

    #[test]
    fn ages_are_one_short_word() {
        let t = |s| Timestamp::from_second(s).unwrap();
        assert_eq!(age(t(100), t(130)), "now");
        assert_eq!(age(t(0), t(180)), "3m");
        assert_eq!(age(t(0), t(7_300)), "2h");
        assert_eq!(age(t(0), t(4 * 86_400)), "4d");
        // A clock that ran backwards still says now.
        assert_eq!(age(t(500), t(0)), "now");
    }
}
