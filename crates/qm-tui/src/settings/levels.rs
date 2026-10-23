//! The Model levels board contains placed model+effort combinations. Add model opens a fixed,
//! searchable catalogue; adding never consumes its entries. Existing combinations move or remove.

use super::model::{BOARD, Model, Settings, combo_label, fallback_tier, tier_heading, tier_label};
use super::paint::Painter;
use super::{BoardItem, SettingsHit, Zone, section};
use crate::app::App;
use crate::text::{cells, truncate};
use crate::theme;
use qm_types::domain::{Effort, Tier};
use ratatui::layout::Rect;
use ratatui::style::Style;

/// Placed combinations in the order the keys walk them: each tier top to bottom.
pub(super) fn items(settings: &Settings) -> Vec<BoardItem> {
    BOARD
        .into_iter()
        .flat_map(|tier| {
            entries(settings, tier).map(move |(model, _, effort)| BoardItem::Placed {
                model,
                tier,
                effort,
            })
        })
        .collect()
}

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

pub(super) fn draw(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let view = &app.view.settings;
    let settings = &view.model;
    let rows: u16 = 3 + BOARD
        .iter()
        .map(|&t| 1 + cells(entries(settings, t).count().max(1)))
        .sum::<u16>();
    let inner = section(area, rows);
    let x = inner.x + 3;
    let end = p.button(x, inner.y, "Add model…", SettingsHit::Find);
    p.text(end + 2, inner.y, "model + effort", theme::dim());
    let mut y = inner.y + 2;
    for tier in BOARD {
        let models: Vec<_> = entries(settings, tier).collect();
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
            view.drag.and_then(|d| d.over) == Some(zone),
        );
        y += 1;
        if models.is_empty() {
            p.text(x + 2, y, &empty_note(settings, tier), theme::dim());
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
    p.hit(
        Rect::new(inner.x, y, inner.width, 1),
        SettingsHit::Zone(Zone::NotInUse),
    );
    heading(
        p,
        x,
        y,
        "DROP HERE TO REMOVE",
        view.drag.and_then(|d| d.over) == Some(Zone::NotInUse),
    );
}

/// The hint beneath an empty tier: where its work runs instead.
fn empty_note(settings: &Settings, tier: Tier) -> String {
    match fallback_tier(settings, tier) {
        Some(to) => format!(
            "empty · {} runs on {}",
            tier_label(tier).to_lowercase(),
            tier_label(to).to_lowercase()
        ),
        None => format!("empty · no available fallback; {tier} requests wait"),
    }
}

/// A tier heading; while a drag hovers its zone it lights up as the drop target.
fn heading(p: &mut Painter<'_, '_>, x: u16, y: u16, text: &str, target: bool) {
    if target {
        let end = p.text(x, y, &format!(" {text} "), theme::button(true));
        p.text(end + 1, y, "release here", theme::accent());
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

/// One placement row: `⠿ name (effort) · account`, with no separate effort control.
fn draw_placed(
    p: &mut Painter<'_, '_>,
    inner: Rect,
    y: u16,
    item: BoardItem,
    m: &Model,
    app: &App,
) {
    let x = inner.x + 3;
    let right = inner.right().saturating_sub(1);
    p.hit(
        Rect::new(inner.x, y, inner.width, 1),
        SettingsHit::Board(item),
    );
    let mut end = p.text(x, y, "⠿ ", theme::dim());
    let label = truncate(
        &combo_label(m, item.effort()),
        usize::from(right.saturating_sub(end)),
    );
    end = p.text(end, y, &label, item_style(app, item));
    let account = truncate(
        &format!(" · {}", m.account),
        usize::from(right.saturating_sub(end)),
    );
    p.text(end, y, &account, theme::dim());
    if app.view.settings.board == Some(item) {
        p.select_row(inner, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popups::Popup;
    use crate::settings::Tab;
    use crate::settings::model::Placement;
    use qm_types::ids::ModelId;
    use ratatui::{Terminal, backend::TestBackend};

    fn settings_with_filled_tiers(mask: u8) -> Settings {
        let mut settings = Settings::default();
        let mut model = Model::new(ModelId::new("test-model").unwrap(), "Test", "Test");
        for (bit, &tier) in Tier::ALL.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                model.placements.push(Placement::new(tier, None));
            }
        }
        if mask != 0 {
            settings.models.push(model);
        }
        settings
    }

    #[test]
    fn every_tier_uses_the_first_permitted_filled_fallback() {
        // Independent expectations: ordinary work never falls back to Premium.
        let cases: [(Tier, &[Tier]); 4] = [
            (Tier::Low, &[Tier::Medium, Tier::High]),
            (Tier::Medium, &[Tier::High, Tier::Low]),
            (Tier::High, &[Tier::Medium, Tier::Low]),
            (Tier::Premium, &[Tier::High, Tier::Medium, Tier::Low]),
        ];
        for mask in 0u8..16 {
            let settings = settings_with_filled_tiers(mask);
            for (tier, permitted) in cases {
                let expected = permitted
                    .iter()
                    .copied()
                    .find(|&t| settings.models.iter().any(|m| m.in_tier(t)));
                assert_eq!(
                    fallback_tier(&settings, tier),
                    expected,
                    "mask {mask}, tier {tier}"
                );
                if !settings.models.iter().any(|m| m.in_tier(tier)) {
                    let want = match expected {
                        Some(to) => format!("empty · {tier} runs on {to}"),
                        None => format!("empty · no available fallback; {tier} requests wait"),
                    };
                    assert_eq!(empty_note(&settings, tier), want, "mask {mask}");
                }
            }
        }
    }

    #[test]
    fn premium_only_board_draws_waiting_hints_at_supported_sizes() {
        for (width, height) in [(150, 45), (120, 40), (119, 40), (100, 30)] {
            for light in [false, true] {
                let mut app = App::new();
                app.open_popup(Popup::Settings);
                app.view.settings.loaded = true;
                app.view.settings.tab = Tab::ModelLevels;
                app.view.settings.model = settings_with_filled_tiers(0b1000);
                app.term.palette = theme::fit_reported(&qm_types::wire::Colors {
                    bg: Some(if light { [255; 3] } else { [0; 3] }),
                    ..Default::default()
                });
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let shown: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect();
                for tier in [Tier::Low, Tier::Medium, Tier::High] {
                    assert!(
                        shown.contains(&format!(
                            "empty · no available fallback; {tier} requests wait"
                        )),
                        "{width}×{height}, light={light}, tier={tier}:\n{shown}"
                    );
                }
                assert!(!shown.contains("runs on premium"));
                assert!(!shown.contains("no model in any tier"));
            }
        }
    }

    #[test]
    fn only_premium_filled_leaves_ordinary_requests_waiting() {
        let settings = settings_with_filled_tiers(0b1000);
        for tier in [Tier::Low, Tier::Medium, Tier::High] {
            assert_eq!(fallback_tier(&settings, tier), None, "{tier}");
            assert_eq!(
                empty_note(&settings, tier),
                format!("empty · no available fallback; {tier} requests wait")
            );
        }
    }
}
