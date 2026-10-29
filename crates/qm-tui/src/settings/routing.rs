//! The Routing tab: the pin, the Quartermaster's own model and effort, Autocompact, and the level
//! each task kind routes to. There is no switch to turn routing off; while a pin is on, a notice
//! says tiers are being skipped.

use super::model::{QuartermasterModel, Settings, effort_label, tier_label};
use super::paint::Painter;
use super::{KIND_ROWS_FROM, SettingsHit, boxed, paragraph, section};
use crate::app::App;
use crate::text::truncate;
use crate::theme;
use qm_types::api::KindTierRow;
use qm_types::config::AutocompactWindow;
use qm_types::domain::{KindRow, Tier};
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
    let inner = section(area, 16);
    let x = inner.x + 1;
    let col = inner.x + COLUMN;
    let right = inner.right().saturating_sub(1);
    let bold = theme::strong();

    let bottom = pin(p, inner, settings, view.routing_row == 0);

    // The Quartermaster: its model and its effort, then what it runs on now.
    let y = bottom + 1;
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
            "Auto tries high → medium → premium → low, at the entry's effort."
        }
        (QuartermasterModel::Auto, Some(_)) => "Auto tries high → medium → premium → low.",
        (_, None) => "Auto effort is its model's tier default.",
        _ => "Its model's supported levels cap the effort.",
    };
    let bottom = wrapped_text(p, col, y + 3, right, explain, theme::dim());
    let applies = "Changes apply when it's next idle. Secondmates follow too.";
    let bottom = wrapped_text(p, col, bottom, right, applies, theme::dim());
    if view.routing_row == 1 {
        p.select_row(inner, y);
    }

    // Autocompact, then Task kinds, then Decisions below them.
    let end = autocompact(p, inner, col, bottom + 1, settings, view.routing_row == 2);
    let end = task_kinds(p, inner, end + 1, settings, view.routing_row);

    // Decisions, not in this version.
    let y = end + 1;
    p.text(x, y, "Decisions", theme::dim().add_modifier(Modifier::BOLD));
    let left = p.text_left_of(right, y, "not in this version", theme::dim());
    let text = truncate(
        "later: a small model that decides task kinds",
        usize::from(left.saturating_sub(col + 1)),
    );
    p.text(col, y, &text, theme::dim());

    draw_notice(p, area, y + 2, settings);
}

/// The pin and its model/fallback controls, returning the first row after them.
fn pin(p: &mut Painter<'_, '_>, inner: Rect, settings: &Settings, selected: bool) -> u16 {
    let col = inner.x + COLUMN;
    let right = inner.right().saturating_sub(1);
    let y = inner.y;
    p.text(inner.x + 1, y, "Always use", theme::strong());
    let switch = if settings.pin.on { "● On" } else { "○ Off" };
    let left = p.button_left_of(right, y, switch, SettingsHit::PinSwitch);
    let bottom = wrapped_text(
        p,
        col,
        y,
        left.saturating_sub(1),
        "one model for all crew work, ignoring tiers",
        theme::text(),
    );
    let choose = |id: &Option<ModelId>| {
        id.as_ref().map_or_else(
            || String::from("Choose…"),
            |id| format!("{} ▾", settings.model_name(id)),
        )
    };
    control(
        p,
        col,
        bottom,
        "model",
        &choose(&settings.pin.model),
        SettingsHit::PinModel,
    );
    control(
        p,
        col,
        bottom + 1,
        "fallback",
        &choose(&settings.pin.fallback),
        SettingsHit::PinFallback,
    );
    if selected {
        p.select_row(inner, y);
    }
    bottom + 2
}

/// Full, independent labels: the conditional build and fix rows are not continuations.
pub(super) const fn kind_name(row: KindRow) -> &'static str {
    match row {
        KindRow::Lookup => "Lookup",
        KindRow::Build => "Build",
        KindRow::BuildOnFinishedPlan => "Build on a plan",
        KindRow::Fix => "Fix",
        KindRow::FixOnConfirmedDiagnosis => "Fix on a diagnosis",
        KindRow::Plan => "Plan",
        KindRow::Research => "Research",
        KindRow::Teach => "Teach",
        KindRow::Review => "Review",
        KindRow::Reproduce => "Reproduce",
        KindRow::Unclear => "Unclear",
    }
}

/// Past the longest name ("Fix on a diagnosis") and a gap. Each grid cell shares this column.
const KIND_VALUE: u16 = 19;
/// A name, the widest tier button, and a cell for the selection marker.
const KIND_CELL_MIN: u16 = 31;
const KIND_GAP: u16 = 2;

/// Under the Task kinds rows: when a change applies and what still comes first.
pub(super) const KINDS_COPY: &str =
    "Applies to the next task routed and to waiting ones. A model you name comes first.";

/// Under a row set to premium: where its work goes when no premium model can serve.
pub(super) const PREMIUM_FALLS_BACK: &str = "then high, medium, low";

/// The note beside a row: where its work runs while its level is empty, else whether it differs
/// from the app's default. Ordinary defaults need no note.
fn kind_note(row: &KindTierRow) -> String {
    let note = match (row.empty, row.runs_on) {
        (true, Some(t)) => format!("(empty, runs on {t})"),
        (true, None) => String::from("(empty, waits)"),
        _ if row.changed() => String::from("changed"),
        _ => String::new(),
    };
    if row.tier == Tier::Premium {
        if note.is_empty() {
            PREMIUM_FALLS_BACK.to_owned()
        } else {
            format!("{note} · {PREMIUM_FALLS_BACK}")
        }
    } else {
        note
    }
}

/// Task kinds in a column-major grid, matching the linear ↑/↓ order. Fold to one column when
/// two whole entries won't fit. Notes sit beside the control when they fit, otherwise below it.
fn task_kinds(
    p: &mut Painter<'_, '_>,
    inner: Rect,
    y: u16,
    settings: &Settings,
    selected: usize,
) -> u16 {
    let pinned = settings.pin.on;
    let (label, name_style) = if pinned {
        (theme::dim().add_modifier(Modifier::BOLD), theme::dim())
    } else {
        (theme::strong(), theme::text())
    };
    p.text(inner.x + 1, y, "Task kinds", label);
    p.button(inner.x + 12, y, "?", SettingsHit::KindHelp);
    let width = inner.width.saturating_sub(2);
    let columns = if width >= KIND_CELL_MIN * 2 + KIND_GAP {
        2
    } else {
        1
    };
    let cell_width = (width.saturating_sub((columns - 1) * KIND_GAP)) / columns;
    let bands = settings.kind_tiers.len().div_ceil(usize::from(columns));
    let mut y = y + 1;
    for band in 0..bands {
        let mut height = 1;
        for column in 0..columns {
            let index = band + usize::from(column) * bands;
            let Some(row) = settings.kind_tiers.get(index) else {
                continue;
            };
            let x = inner.x + 1 + column * (cell_width + KIND_GAP);
            let cell = Rect::new(x, y, cell_width, 1);
            p.text(x + 1, y, kind_name(row.row), name_style);
            let value = format!("{} ▾", tier_label(row.tier));
            let end = p.button(
                x + 1 + KIND_VALUE,
                y,
                &value,
                SettingsHit::KindTier(row.row),
            );
            let note = kind_note(row);
            if !note.is_empty() {
                let room = cell.right().saturating_sub(end + 1);
                if crate::text::width(&note) <= usize::from(room) {
                    p.text(end + 1, y, &note, theme::dim());
                } else {
                    let bottom = wrapped_text(p, x + 1, y + 1, cell.right(), &note, theme::dim());
                    height = height.max(bottom - y);
                }
            }
            if selected == KIND_ROWS_FROM + index {
                p.select_row(cell, y);
            }
        }
        y += height;
    }
    if settings.kind_tiers.iter().any(KindTierRow::changed) {
        p.button(
            inner.x + 1,
            y,
            "Reset all to defaults",
            SettingsHit::ResetKinds,
        );
        y += 1;
    }
    let copy = if pinned {
        "Always use is on, so tiers are skipped until it's off."
    } else {
        KINDS_COPY
    };
    let area = Rect::new(inner.x + 1, y, width.max(1), u16::MAX / 4);
    y + paragraph(p, area, &[(copy.to_owned(), theme::dim())])
}

/// The Autocompact help, under its arrows: what it applies to and when a change lands.
const AUTOCOMPACT_COPY: &str = "Every agent, on any model, compacts its conversation a little before \
    this size, or before its model's window if that is smaller. \
    Changes apply at each agent's next idle moment.";

/// The Autocompact block at `y`: its arrows and value, then what it applies to. Selected when
/// `selected`.
fn autocompact(
    p: &mut Painter<'_, '_>,
    inner: Rect,
    col: u16,
    y: u16,
    settings: &Settings,
    selected: bool,
) -> u16 {
    p.text(inner.x + 1, y, "Autocompact", theme::strong());
    autocompact_arrows(p, col, y, settings.autocompact_window);
    let bottom = wrapped_text(
        p,
        col,
        y + 1,
        inner.right() - 1,
        AUTOCOMPACT_COPY,
        theme::dim(),
    );
    if selected {
        p.select_row(inner, y);
    }
    bottom
}

/// `◂  200k  ▸` at `col` on row `y`. An arrow at the end of the range is dim and not a target.
fn autocompact_arrows(p: &mut Painter<'_, '_>, col: u16, y: u16, window: AutocompactWindow) {
    let (down, up) = (window.down(), window.up());
    if down == window {
        p.text(col, y, " ◂ ", theme::faded());
    } else {
        p.button(col, y, "◂", SettingsHit::AutocompactDown);
    }
    let value = format!(" {} ", window.label());
    let end = p.text(col + 3, y, &value, theme::strong());
    if up == window {
        p.text(end, y, " ▸ ", theme::faded());
    } else {
        p.button(end, y, "▸", SettingsHit::AutocompactUp);
    }
}

/// Wraps a paragraph within these columns, returning the first row after it.
fn wrapped_text(
    p: &mut Painter<'_, '_>,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    style: Style,
) -> u16 {
    let area = Rect::new(x, y, right.saturating_sub(x).max(1), u16::MAX / 4);
    y + paragraph(p, area, &[(text.to_owned(), style)])
}

/// One labelled control on its own row: `label` dim at `col`, its button `VALUE` cells on.
fn control(p: &mut Painter<'_, '_>, col: u16, y: u16, label: &str, value: &str, hit: SettingsHit) {
    p.text(col, y, label, theme::dim());
    p.button(col + VALUE, y, value, hit);
}

/// The notice under the tab while a pin is on, from page row `top`.
fn draw_notice(p: &mut Painter<'_, '_>, area: Rect, top: u16, settings: &Settings) {
    if let Some(notice) = pin_notice(settings)
        && top + 3 <= area.bottom()
    {
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
