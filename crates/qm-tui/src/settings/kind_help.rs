//! Plain-language routing help. Meanings follow `TaskKind`, the Quartermaster prompt, and
//! `qm-routing`'s checked `builds_on` facts; these are explanations, not another rule table.

use crate::app::App;
use crate::hit::{FrameMap, Pane};
use crate::text::wrap;
use crate::theme;
use qm_types::domain::KindRow;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const fn description(row: KindRow) -> &'static str {
    match row {
        KindRow::Lookup => "Find and cite facts that already exist.",
        KindRow::Build => "Create a new feature or capability.",
        KindRow::BuildOnFinishedPlan => "Build using a finished plan from an earlier task.",
        KindRow::Fix => "Repair a bug.",
        KindRow::FixOnConfirmedDiagnosis => {
            "Fix a bug using a confirmed diagnosis or successful reproduction from an earlier task."
        }
        KindRow::Plan => {
            "Write a plan that later work can follow, resolving design choices and steps."
        }
        KindRow::Research => {
            "Investigate how something works, why it happens, or which option fits best."
        }
        KindRow::Teach => "Explain something for you to understand.",
        KindRow::Review => "Independently check another task's work or a pull request.",
        KindRow::Reproduce => "Demonstrate a bug with evidence before trying to fix it.",
        KindRow::Unclear => "Work whose kind isn't clear yet.",
    }
}

/// Every routing row, wrapped without cutting off the explanation.
pub(crate) fn lines(room: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for &row in KindRow::ALL {
        let label = super::routing::kind_name(row);
        let text = format!("{label} — {}", description(row));
        for text in wrap(&text, room.max(1)) {
            if let Some(rest) = text.strip_prefix(label) {
                lines.push(Line::from(vec![
                    Span::styled(label, theme::strong()),
                    Span::styled(rest.to_owned(), theme::text()),
                ]));
            } else {
                lines.push(Line::styled(text, theme::text()));
            }
        }
    }
    lines.push(Line::default());
    lines.extend(wrap("Medium tier means a model + effort combination you ranked in the Medium Model level. Medium effort is the model's effort setting, independent of its tier placement. The same distinction applies to High and Low.", room.max(1))
        .into_iter().map(|text| Line::styled(text, theme::text())));
    lines.push(Line::default());
    lines.extend(wrap("The app checks the earlier task before using either 'on a' row. These are special cases of Build and Fix, not extra task kinds.", room.max(1))
        .into_iter().map(|text| Line::styled(text, theme::dim())));
    lines
}

/// A compact help window; its footer remains visible when small terminals need scrolling.
pub(crate) fn draw(frame: &mut Frame<'_>, inner: Rect, app: &App, map: &mut FrameMap) {
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height.saturating_sub(1),
    );
    let lines = lines(usize::from(body.width));
    let max = lines.len().saturating_sub(usize::from(body.height));
    let scroll = app.view.kind_help_scroll.min(max);
    let shown: Vec<_> = lines
        .into_iter()
        .skip(scroll)
        .take(usize::from(body.height))
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
    map.scroll_areas.push((body, Pane::TaskKinds));
    map.scroll_max.push((Pane::TaskKinds, max));
    if body.height > 0 {
        if scroll > 0 {
            frame.render_widget(
                Paragraph::new("↑").style(theme::dim()),
                Rect::new(body.right(), body.y, 1, 1),
            );
        }
        if scroll < max {
            frame.render_widget(
                Paragraph::new("↓").style(theme::dim()),
                Rect::new(body.right(), body.bottom() - 1, 1, 1),
            );
        }
    }
    if inner.height > 0 {
        frame.render_widget(
            Paragraph::new("↑/↓ scroll · Esc close").style(theme::dim()),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}
