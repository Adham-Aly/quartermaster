//! The Routing tab: the pin and the Quartermaster's own model and effort. There is no switch to
//! turn routing off; while a pin is on, a notice says tiers are being skipped.

use super::model::{QuartermasterModel, Settings, effort_label};
use super::paint::Painter;
use super::{SettingsHit, boxed, paragraph, section};
use crate::app::App;
use crate::text::truncate;
use crate::theme;
use qm_types::ids::ModelId;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

/// Where the second column starts, in cells from the content's left edge: past the longest label
/// ("Quartermaster") and a gap.
const COLUMN: u16 = 17;

/// Where a control's value starts, in cells after its label ("fallback" and a gap).
const VALUE: u16 = 10;

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
    let inner = section(area, 11);
    let x = inner.x + 1;
    let col = inner.x + COLUMN;
    let right = inner.right().saturating_sub(1);
    let bold = theme::strong();

    // The pin, then its model and fallback, one control a row.
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
    control(
        p,
        col,
        y + 1,
        "model",
        &choose(&settings.pin.model),
        SettingsHit::PinModel,
    );
    control(
        p,
        col,
        y + 2,
        "fallback",
        &choose(&settings.pin.fallback),
        SettingsHit::PinFallback,
    );
    if view.routing_row == 0 {
        p.select_row(inner, y);
    }

    // The Quartermaster: its model and its effort, then what it runs on now.
    let y = inner.y + 4;
    p.text(x, y, "Quartermaster", bold);
    let model = match &settings.quartermaster {
        QuartermasterModel::Model(id) => format!("{} ▾", settings.model_name(id)),
        // Auto.
        _ => String::from("Auto ▾"),
    };
    let effort = settings.quartermaster_effort.map_or_else(
        || String::from("Auto ▾"),
        |e| format!("{} ▾", effort_label(e)),
    );
    control(p, col, y, "model", &model, SettingsHit::QuartermasterModel);
    control(
        p,
        col,
        y + 1,
        "effort",
        &effort,
        SettingsHit::QuartermasterEffort,
    );
    let room = usize::from(right.saturating_sub(col));
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
    p.text(col, y + 2, &truncate(&now, room), theme::text());
    let explain = match (&settings.quartermaster, settings.quartermaster_effort) {
        (QuartermasterModel::Auto, None) => {
            "Auto tries high → medium → premium → low, at that tier's effort."
        }
        (QuartermasterModel::Auto, Some(_)) => "Auto tries high → medium → premium → low.",
        (_, None) => "Auto effort is its model's tier default.",
        _ => "Its model's supported levels cap the effort.",
    };
    p.text(col, y + 3, &truncate(explain, room), theme::dim());
    let applies = "Changes apply when it's next idle. Secondmates follow too.";
    p.text(col, y + 4, &truncate(applies, room), theme::dim());
    if view.routing_row == 1 {
        p.select_row(inner, y);
    }

    // Decisions, not in this version.
    let y = inner.y + 10;
    p.text(x, y, "Decisions", theme::dim().add_modifier(Modifier::BOLD));
    let left = p.text_left_of(right, y, "not in this version", theme::dim());
    let text = truncate(
        "later: a small model that decides task kinds",
        usize::from(left.saturating_sub(col + 1)),
    );
    p.text(col, y, &text, theme::dim());

    draw_notice(p, area, inner, settings);
}

/// One labelled control on its own row: `label` dim at `col`, its button `VALUE` cells on.
fn control(p: &mut Painter<'_, '_>, col: u16, y: u16, label: &str, value: &str, hit: SettingsHit) {
    p.text(col, y, label, theme::dim());
    p.button(col + VALUE, y, value, hit);
}

/// The notice under the tab while a pin is on.
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
