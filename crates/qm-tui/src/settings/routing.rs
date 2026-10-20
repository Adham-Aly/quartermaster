//! The Routing tab: the pin and the Quartermaster's own model. There is no switch to turn routing
//! off; while a pin is on, a notice says tiers are being skipped.

use super::model::{QuartermasterModel, Settings, effort_label};
use super::paint::Painter;
use super::{SettingsHit, boxed, paragraph};
use crate::app::App;
use crate::text::truncate;
use crate::theme;
use qm_types::ids::ModelId;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

/// Where the second column starts, in cells from the box's inner left edge (its padding cell):
/// 24 cells after the labels' start.
const COLUMN: u16 = 25;

/// The notice shown here and on the Bridge while a pin is on, if it names a model.
pub(crate) fn pin_notice(settings: &Settings) -> Option<String> {
    let pin = &settings.pin;
    let model = pin.model.as_ref().filter(|_| pin.on)?;
    let name = settings.model_name(model);
    Some(match pin.fallback.as_ref() {
        Some(fallback) => format!(
            "Tiers are skipped: all crew work goes to {name}, then {}.",
            settings.model_name(fallback)
        ),
        None => format!("Tiers are skipped: all crew work goes to {name}."),
    })
}

pub(super) fn draw(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let view = &app.view.settings;
    let settings = &view.model;
    let inner = boxed(p, area, "Routing", 8, theme::dim(), theme::dim());
    let x = inner.x + 1;
    let col = inner.x + COLUMN;
    let right = inner.right().saturating_sub(1);
    let bold = theme::strong();

    // The pin.
    let y = inner.y;
    p.text(x, y, "Always use", bold);
    let switch = if settings.pin.on { "● On" } else { "○ Off" };
    let left = p.button_left_of(right, y, switch, SettingsHit::PinSwitch);
    let text = truncate(
        "one model for all crew work, ignoring tiers",
        usize::from(left.saturating_sub(col + 1)),
    );
    p.text(col, y, &text, theme::text());
    let choose = |id: &Option<ModelId>| {
        id.as_ref().map_or_else(
            || String::from("Choose…"),
            |id| format!("{} ▾", settings.model_name(id)),
        )
    };
    let mut end = p.text(col + 1, y + 1, "model ", theme::dim());
    end = p.button(
        end,
        y + 1,
        &choose(&settings.pin.model),
        SettingsHit::PinModel,
    );
    end = p.text(end + 4, y + 1, "fallback ", theme::dim());
    p.button(
        end,
        y + 1,
        &choose(&settings.pin.fallback),
        SettingsHit::PinFallback,
    );
    if view.routing_row == 0 {
        p.select_row(inner, y);
    }

    // The Quartermaster's model.
    let y = inner.y + 3;
    p.text(x, y, "Quartermaster's model", bold);
    let choice = match &settings.quartermaster {
        QuartermasterModel::Model(id) => format!("{} ▾", settings.model_name(id)),
        // Auto.
        _ => String::from("Auto ▾"),
    };
    let left = p.button_left_of(right, y, &choice, SettingsHit::QuartermasterModel);
    let now = settings.quartermaster_now.as_ref().map_or_else(
        || String::from("now nothing yet"),
        |now| {
            let account = settings
                .model(&now.model)
                .map_or("", |m| m.account.as_str());
            format!(
                "now {} · {account} · {}",
                settings.model_name(&now.model),
                effort_label(now.effort).to_lowercase()
            )
        },
    );
    p.text(
        col,
        y,
        &truncate(&now, usize::from(left.saturating_sub(col + 1))),
        theme::text(),
    );
    let explain = match settings.quartermaster {
        QuartermasterModel::Auto => {
            "Auto tries high → medium → premium → low. Changes apply when it's next idle."
        }
        _ => "Changes apply when it's next idle.",
    };
    let room = usize::from(right.saturating_sub(col + 1));
    p.text(col + 1, y + 1, &truncate(explain, room), theme::dim());
    p.text(
        col + 1,
        y + 2,
        &truncate("Secondmates follow this setting.", room),
        theme::dim(),
    );
    if view.routing_row == 1 {
        p.select_row(inner, y);
    }

    // Decisions, not in this version.
    let y = inner.y + 7;
    p.text(x, y, "Decisions", theme::dim().add_modifier(Modifier::BOLD));
    let left = p.text_left_of(right, y, "not in this version", theme::dim());
    let text = truncate(
        "later: a small model that decides task kinds",
        usize::from(left.saturating_sub(col + 1)),
    );
    p.text(col, y, &text, theme::dim());

    draw_notice(p, area, inner, settings);
}

/// The notice under the box while a pin is on.
fn draw_notice(p: &mut Painter<'_, '_>, area: Rect, inner: Rect, settings: &Settings) {
    if let Some(notice) = pin_notice(settings) {
        // The card goes right under the box.
        let top = inner.bottom() + 1;
        if top + 3 <= area.bottom() {
            let below = Rect::new(area.x, top, area.width, area.bottom() - top);
            let runs: Vec<(String, Style)> = vec![
                (String::from("⚠ Always use is on. "), theme::warn()),
                (notice, theme::text()),
            ];
            let width = usize::from(below.width.saturating_sub(4));
            let rows = crate::text::wrap_styled(&runs, width.max(1)).len();
            let rows = crate::text::cells(rows);
            let inner = boxed(p, below, "", rows, theme::warn(), theme::warn());
            let inner = Rect::new(
                inner.x + 1,
                inner.y,
                inner.width.saturating_sub(2),
                inner.height,
            );
            paragraph(p, inner, &runs);
        }
    }
}
