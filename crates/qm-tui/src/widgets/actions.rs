//! The Actions inbox in the left column: two rows per request waiting on you (what it is and
//! its age, then who asks and about what, dim), newest first after the app's own configuration
//! prompts. A click on a row opens its overlay, the only place its card and buttons are.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, heading_style, section_block};
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

/// Rows each item takes.
pub(crate) const ROWS: usize = 2;

/// Draws the Actions section into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    if area.height == 0 {
        return;
    }
    let mut heading = vec![Span::styled("Actions", heading_style(app, Pane::Actions))];
    if let n @ 1.. = app.snapshot.actions.len() {
        heading.push(Span::raw(" "));
        heading.push(Span::styled(format!(" {n} "), theme::warn_chip()));
    }
    let block = section_block(heading, false);
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
        .flat_map(|(i, item)| {
            let hit = Hit::ActionRow(i);
            let selected = (app.view.selected_action.as_ref() == Some(&item.key))
                .then_some(!app.view.pointer_picked);
            let hovered = selected != Some(true) && app.term.hover == Some(hit);
            lines(item, usize::from(inner_width), now, hovered).map(|line| ListRow {
                line,
                hit: Some(hit),
                selected,
            })
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
        ActionKind::Review => ("✎", theme::accent()),
        ActionKind::Report => ("✓", theme::accent()),
        ActionKind::Question => ("?", theme::warn()),
        ActionKind::Notice => ("!", theme::warn()),
        _ => match item.card.kind {
            CardKind::Approval => ("⚑", theme::accent()),
            _ => ("⚙", theme::warn()),
        },
    }
}

/// Who asked, as a short label: the task, `QM`, `S-n`, or nothing for the app's own prompts.
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

/// An item's two rows: `☰ Plan questions        3h`, then `  T-0044 · plan(auth): session store`.
/// A title with a ` · ` splits there; otherwise the title leads and the second row says who asks.
fn lines(
    item: &ActionItem,
    inner: usize,
    now: jiff::Timestamp,
    hovered: bool,
) -> [Line<'static>; ROWS] {
    let (glyph, style) = glyph(item);
    let age = age(item.at, now);
    let text_style = if hovered {
        theme::strong()
    } else {
        theme::text()
    };
    let (what, about) = item
        .title
        .split_once(" · ")
        .map_or((item.title.as_str(), None), |(what, about)| {
            (what, Some(about))
        });
    let source = source(item).filter(|source| !what.starts_with(source.as_str()));
    let detail = match (source, about) {
        (Some(source), Some(about)) => format!("{source} · {about}"),
        (Some(source), None) => source,
        (None, Some(about)) => about.to_owned(),
        (None, None) => kind_word(item).to_owned(),
    };
    // Glyph, space, words, at least one space, the age.
    let room = inner.saturating_sub(2 + 1 + width(&age));
    let what = truncate(what, room);
    let gap = inner.saturating_sub(2 + width(&what) + width(&age)).max(1);
    [
        Line::from(vec![
            Span::styled(glyph, style),
            Span::raw(" "),
            Span::styled(what, text_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(age, theme::dim()),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(truncate(&detail, inner.saturating_sub(2)), theme::dim()),
        ]),
    ]
}

/// What an item is, for a second row with nothing else to say.
fn kind_word(item: &ActionItem) -> &'static str {
    match item.kind {
        ActionKind::Approval => "approval",
        ActionKind::Decision => "decision",
        ActionKind::Plan => "plan",
        ActionKind::Question => "question",
        ActionKind::Notice => "notice",
        _ => "setup",
    }
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
