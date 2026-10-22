//! The Model levels tab: a board of tiers. Each row is a tier entry: a model plus an effort level,
//! so one model can sit in several tiers, and in one tier more than once at different efforts.
//! Drag an entry between tiers or select it and press `m`; its effort offers only the levels that
//! model supports. An empty tier says where its work goes, or that Premium requests wait.

use super::model::{
    BOARD, Model, Settings, effort_label, fallback_tier, model_tier_default, tier_heading,
    tier_label,
};
use super::paint::Painter;
use super::{BoardItem, SettingsHit, Zone, section};
use crate::app::App;
use crate::text::{cells, truncate, width};
use crate::theme;
use qm_types::domain::{Effort, Tier};
use ratatui::layout::Rect;
use ratatui::style::Style;

/// Cells between two items on the NOT IN USE row.
const UNUSED_GAP: u16 = 6;

/// The board's items in the order the keys walk them: each tier top to bottom, then NOT IN USE.
pub(super) fn items(settings: &Settings) -> Vec<BoardItem> {
    let mut out = Vec::new();
    for tier in BOARD {
        out.extend(
            entries(settings, tier).map(|(model, _, effort)| BoardItem::Placed {
                model,
                tier,
                effort,
            }),
        );
    }
    for (model, m) in settings.models.iter().enumerate() {
        if m.unused() && m.listed {
            out.push(BoardItem::Unused { model });
        }
    }
    out
}

/// A tier's entries, board order (by model, then in the order they were made): the model's index,
/// the model and the entry's effort.
fn entries(
    settings: &Settings,
    tier: Tier,
) -> impl Iterator<Item = (usize, &Model, Option<Effort>)> {
    settings
        .models
        .iter()
        .enumerate()
        .flat_map(move |(i, m)| m.placements_in(tier).map(move |p| (i, m, p.effort)))
}

/// "⠿ GPT-6.1 Sol · ChatGPT Pro" and, for a new model, " new".
fn label_width(m: &Model) -> u16 {
    let new = if m.new { 4 } else { 0 };
    cells(width(&m.name) + width(&m.account) + 5 + new)
}

/// The NOT IN USE items laid out in rows `room` cells wide: (row, column offset, item).
fn flow(settings: &Settings, room: u16) -> Vec<(u16, u16, usize)> {
    let mut out = Vec::new();
    let (mut row, mut x) = (0u16, 0u16);
    for (model, m) in settings.models.iter().enumerate() {
        if !(m.unused() && m.listed) {
            continue;
        }
        let w = label_width(m);
        if x > 0 && x + UNUSED_GAP + w > room {
            row += 1;
            x = 0;
        }
        if x > 0 {
            x += UNUSED_GAP;
        }
        out.push((row, x, model));
        x += w;
    }
    out
}

pub(super) fn draw(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let view = &app.view.settings;
    let settings = &view.model;
    let room = area.width.saturating_sub(7);
    let unused = flow(settings, room);
    let unused_rows = unused.last().map_or(0, |&(row, _, _)| row + 1);
    let tier_rows: u16 = BOARD
        .iter()
        .map(|&t| 1 + cells(entries(settings, t).count().max(1)))
        .sum();
    let rows = tier_rows + 1 + unused_rows + 1;
    let inner = section(area, rows);
    // The board is indented two cells in from the padding.
    let x = inner.x + 3;
    let right = inner.right().saturating_sub(1);
    let dragging = view.drag;
    let mut y = inner.y;

    for tier in BOARD {
        let models: Vec<(usize, &Model, Option<Effort>)> = entries(settings, tier).collect();
        let zone = Zone::Tier(tier);
        let height = 1 + cells(models.len().max(1));
        p.hit(
            Rect::new(inner.x, y, inner.width, height),
            SettingsHit::Zone(zone),
        );
        heading(
            p,
            x,
            y,
            tier_heading(tier),
            dragging.and_then(|d| d.over) == Some(zone),
        );
        y += 1;
        if models.is_empty() {
            let note = empty_note(settings, tier);
            p.text(x + 2, y, &note, theme::dim());
            y += 1;
        }
        for (model, m, effort) in models {
            let item = BoardItem::Placed {
                model,
                tier,
                effort,
            };
            draw_placed(p, inner, y, item, m, app);
            y += 1;
        }
    }

    let zone = Zone::NotInUse;
    p.hit(
        Rect::new(inner.x, y, inner.width, 1 + unused_rows),
        SettingsHit::Zone(zone),
    );
    heading(
        p,
        x,
        y,
        "NOT IN USE",
        dragging.and_then(|d| d.over) == Some(zone),
    );
    y += 1;
    draw_unused(p, x, y, &unused, app);
    y += unused_rows;
    let end = p.button(x, y, "Find another model…", SettingsHit::Find);
    let more = settings.unlisted();
    if more > 0 {
        let text = format!("{more} more on your accounts");
        let text = truncate(&text, usize::from(right.saturating_sub(end + 2)));
        p.text(end + 2, y, &text, theme::dim());
    }
}

/// The hint beneath an empty tier; Premium never falls back to another tier.
fn empty_note(settings: &Settings, tier: Tier) -> String {
    if tier == Tier::Premium {
        return String::from("empty · premium requests wait");
    }
    match fallback_tier(settings, tier) {
        Some(to) => format!(
            "empty · {} runs on {}",
            tier_label(tier).to_lowercase(),
            tier_label(to).to_lowercase()
        ),
        None => String::from("empty · no model in any tier yet"),
    }
}

/// The NOT IN USE items, flowed in rows from `(x, y)`.
fn draw_unused(p: &mut Painter<'_, '_>, x: u16, y: u16, unused: &[(u16, u16, usize)], app: &App) {
    let view = &app.view.settings;
    let settings = &view.model;
    for &(row, offset, model) in unused {
        let m = &settings.models[model];
        let item = BoardItem::Unused { model };
        let at = x + offset;
        let row_y = y + row;
        let style = item_style(app, item);
        let mut end = p.text(at, row_y, "⠿ ", theme::dim());
        end = p.text(end, row_y, &m.name, style);
        end = p.text(end, row_y, &format!(" · {}", m.account), theme::dim());
        if m.new {
            end = p.text(end, row_y, " new", theme::accent());
        }
        p.hit(
            Rect::new(at, row_y, end.saturating_sub(at), 1),
            SettingsHit::Board(item),
        );
        if view.board == Some(item) {
            p.select_row(Rect::new(at, row_y, end.saturating_sub(at), 1), row_y);
        }
    }
}

/// A tier heading; while a drag hovers its zone it lights up as the drop target.
fn heading(p: &mut Painter<'_, '_>, x: u16, y: u16, text: &str, target: bool) {
    if target {
        let end = p.text(x, y, &format!(" {text} "), theme::button(true));
        p.text(end + 1, y, "drop to place here", theme::accent());
    } else {
        p.text(x, y, text, theme::warn());
    }
}

/// The model name's style: dim while it is the one being dragged.
fn item_style(app: &App, item: BoardItem) -> Style {
    if app
        .view
        .settings
        .drag
        .is_some_and(|d| d.item == item && d.over.is_some())
    {
        theme::dim()
    } else {
        theme::text()
    }
}

/// One placement row: `⠿ name · account` on the left, `effort [level ▾]` on the right.
fn draw_placed(
    p: &mut Painter<'_, '_>,
    inner: Rect,
    y: u16,
    item: BoardItem,
    m: &Model,
    app: &App,
) {
    let Some(tier) = item.tier() else { return };
    let Some(placement) = m.placement(tier, item.effort()) else {
        return;
    };
    let x = inner.x + 3;
    let right = inner.right().saturating_sub(1);
    p.hit(
        Rect::new(inner.x, y, inner.width, 1),
        SettingsHit::Board(item),
    );

    // The effort control, right-aligned.
    let (label, own) = match placement.effort {
        Some(effort) => (format!("{} ▾", effort_label(effort)), true),
        None => (
            format!(
                "Tier default · {} ▾",
                effort_label(model_tier_default(
                    m,
                    tier,
                    &app.view.settings.model.tier_efforts
                ))
            ),
            false,
        ),
    };
    let mut edge = right;
    if own {
        edge = p.text_left_of(edge, y, "your setting", theme::dim()) - 1;
    }
    edge = p.button_left_of(edge, y, &label, SettingsHit::Effort(item));
    edge = p.text_left_of(edge.saturating_sub(1), y, "effort", theme::text());

    let mut end = p.text(x, y, "⠿ ", theme::dim());
    let room = usize::from(edge.saturating_sub(end + 2));
    let name = truncate(&m.name, room);
    end = p.text(end, y, &name, item_style(app, item));
    let room = usize::from(edge.saturating_sub(end + 2));
    let account = truncate(&format!(" · {}", m.account), room);
    p.text(end, y, &account, theme::dim());
    if app.view.settings.board == Some(item) {
        p.select_row(inner, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::model::Placement;
    use qm_types::ids::ModelId;

    #[test]
    fn empty_premium_always_waits_including_with_only_high_configured() {
        // Every combination of non-Premium tiers, including none and High alone.
        for mask in 0u8..8 {
            let mut settings = Settings::default();
            let mut model = Model::new(ModelId::new("test-model").unwrap(), "Test", "Test");
            for (bit, tier) in [Tier::High, Tier::Medium, Tier::Low]
                .into_iter()
                .enumerate()
            {
                if mask & (1 << bit) != 0 {
                    model.placements.push(Placement::new(tier, None));
                }
            }
            if mask != 0 {
                settings.models.push(model);
            }
            assert_eq!(fallback_tier(&settings, Tier::Premium), None);
            let note = empty_note(&settings, Tier::Premium);
            assert_eq!(note, "empty · premium requests wait", "mask {mask}");
            assert!(!note.contains("runs on"));
        }
    }
}
