//! Needs you, in the left column under Projects: every request waiting on you, newest first
//! after the app's own configuration prompts. Each is its title, wrapped to two lines, then a dim
//! line saying what kind it is, who asks and how long ago, all behind a bar in its colour:
//!
//! ```text
//!  Needs you  2  ───━━━──────
//! ▌ Should I fix the small typo in
//! ▌ the README while I'm there?
//! ▌ question · Quartermaster · 1h
//! ```
//!
//! While something there is new to you the section moves (`widgets::beacon`). A click on an item
//! opens its overlay, the only place its card and buttons are. (The pane is still `Pane::Actions`
//! in the code.)

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, section_block};
use crate::hit::{FrameMap, Hit, Pane};
use crate::router::Screen;
use crate::text::{truncate, width, wrap};
use crate::theme;
use crate::widgets::beacon;
use qm_types::api::{ActionItem, ActionKind, CardKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;

/// The empty section's one row.
pub(crate) const EMPTY: &str = "Nothing needs you.";

/// The section's name.
pub(crate) const NAME: &str = "Needs you";

/// The most lines an item's title wraps to.
const TITLE_LINES: usize = 2;

/// Draws the section into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    if area.height == 0 {
        return;
    }
    let count = app.snapshot.actions.len();
    let focused = app.screen == Screen::Bridge && app.focus == Pane::Actions;
    let style = if focused {
        theme::accent().add_modifier(Modifier::BOLD)
    } else if count > 0 {
        // Not the warn colour: in 16 colours it is focus's, and the chip says it anyway.
        theme::text().add_modifier(Modifier::BOLD)
    } else if app.term.hover == Some(Hit::Pane(Pane::Actions)) {
        theme::text()
    } else {
        theme::title()
    };
    let mut heading = vec![Span::styled(NAME, style)];
    if count > 0 {
        let lit = beacon::lit(app);
        heading.push(Span::raw(" "));
        heading.push(beacon::chip(app, count, lit));
        if lit {
            // The heading starts a cell in; the rule leaves a cell at the end.
            let used = 1 + width(NAME) + 1 + width(&format!(" {count} ")) + 1;
            let cells = usize::from(area.width).saturating_sub(used + 1);
            heading.push(Span::raw(" "));
            heading.extend(beacon::rule(app, cells));
        }
    }
    let block = section_block(heading, true);
    let built = build(app, block.inner(area).width);
    map.action_spots = built.spots;
    draw_list(
        frame,
        area,
        block,
        built.rows,
        app.view.actions_scroll,
        Pane::Actions,
        map,
    );
}

/// The section as drawn: its rows, and each item's lines in them.
pub(crate) struct Built {
    pub(crate) rows: Vec<ListRow>,
    pub(crate) spots: Vec<Range<usize>>,
}

/// The section's row count at `inner_width`, which sizes it.
pub(crate) fn height(app: &App, inner_width: u16) -> usize {
    build(app, inner_width).rows.len()
}

/// The section's rows at `inner_width`.
pub(crate) fn build(app: &App, inner_width: u16) -> Built {
    let items = &app.snapshot.actions;
    if items.is_empty() {
        return Built {
            rows: empty_lines(&[EMPTY], inner_width)
                .into_iter()
                .map(ListRow::plain)
                .collect(),
            spots: Vec::new(),
        };
    }
    let now = jiff::Timestamp::now();
    let mut out = Built {
        rows: Vec::new(),
        spots: Vec::new(),
    };
    for (i, item) in items.iter().enumerate() {
        let hit = Hit::ActionRow(i);
        let selected = (app.view.selected_action.as_ref() == Some(&item.key))
            .then_some(!app.view.pointer_picked);
        let hovered = selected != Some(true) && app.term.hover == Some(hit);
        let from = out.rows.len();
        for line in lines(app, item, usize::from(inner_width), now, hovered) {
            out.rows.push(ListRow {
                line,
                hit: Some(hit),
                selected,
            });
        }
        out.spots.push(from..out.rows.len());
    }
    out
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

/// Who asked: retained friendly name, live crew name, then a generic crewmate label.
pub(crate) fn source(app: &App, item: &ActionItem) -> Option<String> {
    crate::presentation::Presenter(&app.snapshot).source(item)
}

/// The list and overlay use the same source, never an ID fallback.
fn asker(app: &App, item: &ActionItem) -> Option<String> {
    source(app, item)
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

/// An item's lines: its title wrapped to two lines, then `question · Quartermaster · 1h`, each
/// behind the bar. A title with a ` · ` splits there, the rest leading the dim line.
fn lines(
    app: &App,
    item: &ActionItem,
    inner: usize,
    now: jiff::Timestamp,
    hovered: bool,
) -> Vec<Line<'static>> {
    let (_, style) = glyph(item);
    let bar = beacon::bar(app, item, style);
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
    let room = inner.saturating_sub(2).max(1);
    let mut title = wrap(what, room);
    if title.len() > TITLE_LINES {
        let rest = title[TITLE_LINES - 1..].join(" ");
        title.truncate(TITLE_LINES - 1);
        title.push(truncate(&rest, room));
    }
    let age = age(item.at, now);
    let about = about.map_or_else(|| kind_word(item).to_owned(), str::to_owned);
    // Who asks is left out when the title already says it, or when it doesn't fit whole.
    let asker = asker(app, item).filter(|who| {
        !what.to_lowercase().contains(&who.to_lowercase())
            && width(&about) + width(who) + width(&age) + 6 <= room
    });
    let words = match asker {
        Some(who) => format!("{about} · {who}"),
        None => about,
    };
    let detail = format!(
        "{} · {age}",
        truncate(&words, room.saturating_sub(3 + width(&age)))
    );
    let mut out: Vec<Line<'static>> = title
        .into_iter()
        .map(|part| {
            Line::from(vec![
                bar.clone(),
                Span::raw(" "),
                Span::styled(part, text_style),
            ])
        })
        .collect();
    out.push(Line::from(vec![
        bar,
        Span::raw(" "),
        Span::styled(detail, theme::dim()),
    ]));
    out
}

/// What an item is, for a dim line with nothing else to say.
fn kind_word(item: &ActionItem) -> &'static str {
    match item.kind {
        ActionKind::Approval => "approval",
        ActionKind::Decision => "decision",
        ActionKind::Plan => "plan",
        ActionKind::Question => "question",
        ActionKind::Notice => "notice",
        ActionKind::Review => "review",
        ActionKind::Report => "report",
        _ => "setup",
    }
}
