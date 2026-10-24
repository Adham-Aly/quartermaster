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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hit::{Hit, Hits};
    use crate::settings::Tab;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use qm_types::api::Command;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use std::time::Instant;

    /// A Settings popup on Routing with the window at `tokens`.
    fn app_at(tokens: u32) -> App {
        let mut app = App::new();
        app.view.settings.loaded = true;
        app.view.settings.tab = Tab::Routing;
        app.view.settings.model.autocompact_window = AutocompactWindow::new(tokens);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_settings_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
    }

    /// The window sizes the screens queued for the core, in order.
    fn sent(app: &App) -> Vec<u32> {
        app.view
            .settings
            .outbox
            .iter()
            .filter_map(|c| match c {
                Command::SetAutocompactWindow { tokens } => Some(*tokens),
                _ => None,
            })
            .collect()
    }

    /// Draws the Routing tab into a `width`×`height` screen: the screen, and the hits recorded.
    fn render(app: &App, width: u16, height: u16, filled: bool) -> (Buffer, Hits) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hits = Hits::default();
        let area = Rect::new(0, 0, width, height);
        terminal
            .draw(|frame| {
                let mut p = Painter::new(frame, &mut hits, None, filled, area);
                draw(&mut p, area, app);
            })
            .unwrap();
        (terminal.backend().buffer().clone(), hits)
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        let area = buffer.area;
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    /// The columns of the Autocompact arrows, and their row: the row holding both `◂` and `▸`
    /// (the selected row's `▸` is in the first column, on another row).
    fn arrows(buffer: &Buffer) -> Option<(u16, u16, u16)> {
        rows(buffer).iter().enumerate().find_map(|(y, row)| {
            let column = |byte: usize| u16::try_from(row[..byte].chars().count()).ok();
            let left = column(row.find('◂')?)?;
            let right = column(row.find('▸')?)?;
            Some((left, right, u16::try_from(y).ok()?))
        })
    }

    fn target(hits: &Hits, hit: SettingsHit) -> Option<Rect> {
        hits.rect_of(Hit::Settings(hit))
    }

    #[test]
    fn keys_step_the_window_and_select_its_row() {
        let mut app = app_at(200_000);
        press(&mut app, KeyCode::Char('+'));
        press(&mut app, KeyCode::Char('='));
        press(&mut app, KeyCode::Char('-'));
        assert_eq!(sent(&app), [250_000, 300_000, 250_000]);
        assert_eq!(app.view.settings.routing_row, 2);
    }

    #[test]
    fn arrows_select_the_row_and_left_right_still_switch_tabs() {
        let mut app = app_at(200_000);
        for _ in 0..=super::super::ROUTING_ROWS {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(
            app.view.settings.routing_row,
            super::super::ROUTING_ROWS - 1,
            "stops at the last row"
        );
        press(&mut app, KeyCode::Up);
        assert_eq!(
            app.view.settings.routing_row,
            super::super::ROUTING_ROWS - 2
        );
        press(&mut app, KeyCode::Right);
        assert_eq!(app.view.settings.tab, Tab::Projects);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.view.settings.tab, Tab::Routing);
    }

    #[test]
    fn keys_at_the_ends_send_nothing() {
        let mut app = app_at(AutocompactWindow::MIN);
        press(&mut app, KeyCode::Char('-'));
        assert_eq!(sent(&app), Vec::<u32>::new());
        let mut app = app_at(AutocompactWindow::MAX);
        press(&mut app, KeyCode::Char('+'));
        assert_eq!(sent(&app), Vec::<u32>::new());
    }

    #[test]
    fn clicks_step_the_window() {
        let mut app = app_at(200_000);
        let now = Instant::now();
        app.settings_click(SettingsHit::AutocompactUp, false, now);
        app.settings_click(SettingsHit::AutocompactDown, false, now);
        app.settings_click(SettingsHit::AutocompactDown, false, now);
        assert_eq!(sent(&app), [250_000, 200_000, 150_000]);
        assert_eq!(app.view.settings.routing_row, 2);
    }

    #[test]
    fn an_arrow_is_a_target_inside_the_range() {
        let (buffer, hits) = render(&app_at(200_000), 100, 30, false);
        let (left, right, y) = arrows(&buffer).unwrap();
        let down = target(&hits, SettingsHit::AutocompactDown).unwrap();
        let up = target(&hits, SettingsHit::AutocompactUp).unwrap();
        assert_eq!((down.x + 1, down.y), (left, y));
        assert_eq!((up.x + 1, up.y), (right, y));
    }

    #[test]
    fn the_arrow_at_the_bottom_end_is_dim_and_not_a_target() {
        let (buffer, hits) = render(&app_at(AutocompactWindow::MIN), 100, 30, false);
        let (left, _, y) = arrows(&buffer).unwrap();
        assert_eq!(target(&hits, SettingsHit::AutocompactDown), None);
        assert!(target(&hits, SettingsHit::AutocompactUp).is_some());
        assert_eq!(buffer[(left, y)].symbol(), "◂");
        assert_eq!(buffer[(left, y)].fg, theme::dim().fg.unwrap());
        assert!(buffer[(left, y)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn the_dim_end_stays_dim_on_a_selected_row() {
        // The fill turns dim colours into plain text, so the end arrow keeps its faint attribute.
        for (tokens, dim_at_left) in [
            (AutocompactWindow::MIN, true),
            (AutocompactWindow::MAX, false),
        ] {
            let mut app = app_at(tokens);
            app.view.settings.routing_row = 2;
            let (buffer, _) = render(&app, 100, 30, true);
            let (left, right, y) = arrows(&buffer).unwrap();
            let (dim, live) = if dim_at_left {
                (left, right)
            } else {
                (right, left)
            };
            assert!(
                buffer[(dim, y)].modifier.contains(Modifier::DIM),
                "{tokens}"
            );
            assert!(
                !buffer[(live, y)].modifier.contains(Modifier::DIM),
                "{tokens}"
            );
        }
    }

    #[test]
    fn the_arrow_at_the_top_end_is_dim_and_not_a_target() {
        let (buffer, hits) = render(&app_at(AutocompactWindow::MAX), 100, 30, false);
        let (_, right, y) = arrows(&buffer).unwrap();
        assert_eq!(target(&hits, SettingsHit::AutocompactUp), None);
        assert!(target(&hits, SettingsHit::AutocompactDown).is_some());
        assert_eq!(buffer[(right, y)].symbol(), "▸");
        assert_eq!(buffer[(right, y)].fg, theme::dim().fg.unwrap());
        assert!(buffer[(right, y)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn the_copy_wraps_as_one_paragraph_at_100_columns() {
        let (buffer, _) = render(&app_at(200_000), 100, 30, false);
        let screen = rows(&buffer);
        let start = screen
            .iter()
            .position(|r| r.contains("Every agent"))
            .unwrap();
        let expected = crate::text::wrap_styled(
            &[(AUTOCOMPACT_COPY.to_owned(), theme::dim())],
            100 - usize::from(COLUMN) - 1,
        );
        for (row, line) in expected.iter().enumerate() {
            let text: String = line.iter().map(|(text, _)| text.as_str()).collect();
            assert!(screen[start + row].contains(&text), "{}", screen.join("\n"));
        }
        assert!(screen.iter().any(|row| row.contains("◂  200k  ▸")));
        assert!(screen.iter().any(|row| row.contains("Autocompact")));
    }

    #[test]
    fn auto_explains_entry_effort_not_tier_default_at_supported_widths() {
        for width in [150, 120, 119, 100] {
            let (buffer, _) = render(&app_at(200_000), width, 30, false);
            let screen = rows(&buffer).join("\n");
            assert!(
                screen.contains("at the entry's effort."),
                "{width}:\n{screen}"
            );
            assert!(!screen.contains("tier's effort"));
        }
    }

    #[test]
    fn the_routing_hints_fit_at_100_columns() {
        // The status bar drops its right-hand hints unless they fit beside the tab's hint.
        let left = " click to change".chars().count() + 2;
        let keys = crate::settings::hints(Tab::Routing).1.chars().count() + 1;
        assert!(
            left + keys <= 100,
            "{}",
            crate::settings::hints(Tab::Routing).1
        );
    }

    /// A Settings popup on Routing with every Task kinds row at its default, `review` at `tier`.
    fn app_with_review(tier: Tier) -> App {
        let mut app = app_at(200_000);
        app.view.settings.model.kind_tiers = KindRow::ALL
            .iter()
            .map(|&row| {
                let t = if row == KindRow::Review {
                    tier
                } else {
                    row.default_tier()
                };
                KindTierRow::new(row, t, row.default_tier())
            })
            .collect();
        app
    }

    fn kind_commands(app: &App) -> Vec<Command> {
        app.view
            .settings
            .outbox
            .iter()
            .filter(|c| matches!(c, Command::SetKindTier { .. } | Command::ResetKindTiers))
            .cloned()
            .collect()
    }

    fn review_row() -> usize {
        KIND_ROWS_FROM
            + KindRow::ALL
                .iter()
                .position(|&r| r == KindRow::Review)
                .unwrap()
    }

    #[test]
    fn every_kind_row_draws_whole_at_the_smallest_width() {
        // Page widths, not terminal widths: one column below 66, two at and above it.
        for width in [55, 65, 66, 67, 98, 144] {
            let (buffer, hits) = render(&app_with_review(Tier::Premium), width, 60, false);
            let screen = rows(&buffer).join("\n");
            for &row in KindRow::ALL {
                let name = kind_name(row).trim_start();
                assert!(screen.contains(name), "{name} at {width}:\n{screen}");
                assert!(target(&hits, SettingsHit::KindTier(row)).is_some(), "{row}");
            }
            let review = target(&hits, SettingsHit::KindTier(KindRow::Review)).unwrap();
            let screen_rows = rows(&buffer);
            assert!(screen_rows[usize::from(review.y)].contains("Premium ▾"));
            // Notes stay inside their own grid cell and preserve the full fallback explanation.
            let note_x = review.x - KIND_VALUE;
            let note_width = if width >= 66 {
                (width - 4) / 2 - 1
            } else {
                width - 3
            };
            let note: String = (review.y..review.y + 3)
                .map(|y| {
                    (note_x..note_x + note_width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" ");
            let words = note.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(words.contains("changed"), "{width}: {words}");
            assert!(words.contains(PREMIUM_FALLS_BACK), "{width}: {words}");
            let reset_y = target(&hits, SettingsHit::ResetKinds).unwrap().y;
            assert!(
                !screen_rows[..usize::from(reset_y)]
                    .join(" ")
                    .contains("default")
            );
            let lookup = target(&hits, SettingsHit::KindTier(KindRow::Lookup)).unwrap();
            let build = target(&hits, SettingsHit::KindTier(KindRow::Build)).unwrap();
            let planned =
                target(&hits, SettingsHit::KindTier(KindRow::BuildOnFinishedPlan)).unwrap();
            assert_eq!(lookup.x, planned.x);
            assert_eq!(lookup.x, build.x);
            assert_eq!(build.y, lookup.y + 1, "defaults need no note row");
            let research = target(&hits, SettingsHit::KindTier(KindRow::Research)).unwrap();
            if width >= 66 {
                assert_eq!(lookup.y, research.y);
                assert!(research.x > lookup.x);
                let teach = target(&hits, SettingsHit::KindTier(KindRow::Teach)).unwrap();
                assert_eq!(research.x, teach.x);
                assert_eq!(teach.y, build.y);
            } else {
                assert_eq!(lookup.x, research.x);
                assert!(research.y > planned.y);
            }
            assert!(screen.contains("Task kinds"), "{screen}");
            assert!(screen.contains("Reset all to defaults"), "{screen}");
            assert!(screen.contains("Decisions"), "{screen}");
        }
    }

    #[test]
    fn notes_inline_at_an_exact_fit_and_wrap_one_cell_below_it() {
        for (tier, empty, runs_on, note) in [
            (Tier::Low, false, None, "changed"),
            (
                Tier::High,
                true,
                Some(Tier::Medium),
                "(empty, runs on medium)",
            ),
            (Tier::High, true, None, "(empty, waits)"),
            (
                Tier::Premium,
                false,
                None,
                "changed · then high, medium, low",
            ),
        ] {
            let mut app = app_with_review(tier);
            let review = app
                .view
                .settings
                .model
                .kind_tiers
                .iter_mut()
                .find(|r| r.row == KindRow::Review)
                .unwrap();
            review.empty = empty;
            review.runs_on = runs_on;
            // Force a single cell, then make its right edge an exact fit for the note.
            app.view.settings.model.kind_tiers = vec![review.clone()];
            let width = 1
                + 1
                + KIND_VALUE
                + crate::widgets::button::width_of(&format!("{} ▾", tier_label(tier)))
                + 1
                + crate::text::cells(crate::text::width(note))
                + 1;
            // Account for the two-column breakpoint when finding the exact-fit cell width.
            let width = if width >= 66 { width * 2 } else { width };
            for (width, inline) in [(width, true), (width - 1, false)] {
                let (buffer, hits) = render(&app, width, 60, false);
                let review = target(&hits, SettingsHit::KindTier(KindRow::Review)).unwrap();
                let screen = rows(&buffer);
                assert_eq!(
                    screen[usize::from(review.y)].contains(note),
                    inline,
                    "{width}:\n{}",
                    screen.join("\n")
                );
                if !inline {
                    assert!(
                        screen[usize::from(review.y + 1)].contains(note),
                        "{width}:\n{}",
                        screen.join("\n")
                    );
                }
            }
        }
    }

    #[test]
    fn reset_all_shows_only_while_a_row_differs() {
        let (buffer, hits) = render(&app_with_review(Tier::High), 98, 60, false);
        assert!(!rows(&buffer).join("\n").contains("Reset all"));
        assert_eq!(target(&hits, SettingsHit::ResetKinds), None);
    }

    #[test]
    fn an_empty_level_says_where_its_work_runs() {
        let mut app = app_with_review(Tier::Low);
        let review = app
            .view
            .settings
            .model
            .kind_tiers
            .iter_mut()
            .find(|r| r.row == KindRow::Review)
            .unwrap();
        review.empty = true;
        review.runs_on = Some(Tier::Medium);
        let (buffer, hits) = render(&app, 98, 60, false);
        let target = target(&hits, SettingsHit::KindTier(KindRow::Review)).unwrap();
        let screen = rows(&buffer);
        assert!(screen[usize::from(target.y)].contains("Low ▾"));
        assert!(screen[usize::from(target.y + 1)].contains("(empty, runs on medium)"));
    }

    #[test]
    fn the_rows_are_dim_while_a_pin_is_on() {
        let mut app = app_with_review(Tier::High);
        app.view.settings.model.pin.on = true;
        let (buffer, _) = render(&app, 98, 60, false);
        let screen = rows(&buffer);
        let y = screen.iter().position(|r| r.contains("Lookup")).unwrap();
        let x = screen[y].find("Lookup").unwrap();
        let x = u16::try_from(screen[y][..x].chars().count()).unwrap();
        let y = u16::try_from(y).unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::dim().fg.unwrap());
        assert!(screen.iter().any(|r| r.contains("Always use is on")));
    }

    #[test]
    fn keys_pick_a_level_and_reset_a_row() {
        let mut app = app_with_review(Tier::High);
        for _ in 0..review_row() {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(app.view.settings.routing_row, review_row());
        press(&mut app, KeyCode::Enter);
        let picker = app.view.settings.picker.as_ref().unwrap();
        assert_eq!(picker.options[0].0, "Default (high)");
        assert_eq!(picker.cursor, 0, "a row at its default has Default chosen");
        // Default, Premium: choose premium.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('r'));
        // `r` on a row at its default sends nothing more.
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(
            kind_commands(&app),
            [
                Command::SetKindTier {
                    row: KindRow::Review,
                    tier: Some(Tier::Premium)
                },
                Command::SetKindTier {
                    row: KindRow::Review,
                    tier: None
                },
            ]
        );
    }

    #[test]
    fn a_click_on_a_level_opens_its_picker() {
        let mut app = app_with_review(Tier::High);
        app.settings_click(SettingsHit::KindTier(KindRow::Teach), false, Instant::now());
        assert_eq!(
            app.view.settings.picker.as_ref().map(|p| p.purpose.clone()),
            Some(super::super::PickFor::KindTier(KindRow::Teach))
        );
        assert_eq!(app.view.settings.routing_row, KIND_ROWS_FROM + 7);
    }

    #[test]
    fn reset_all_waits_for_a_yes() {
        let mut app = app_with_review(Tier::Premium);
        press(&mut app, KeyCode::Char('R'));
        assert!(app.view.settings.confirm.is_some());
        assert_eq!(
            crate::settings::status_hints(&app).1,
            "Reset all task kinds to defaults? ⏎ yes · esc no"
        );
        assert_eq!(kind_commands(&app), []);
        press(&mut app, KeyCode::Esc);
        assert!(app.view.settings.confirm.is_none());
        assert!(
            app.view.settings.loaded,
            "esc only said no; the popup stays"
        );
        assert_eq!(kind_commands(&app), []);

        app.settings_click(SettingsHit::ResetKinds, false, Instant::now());
        assert!(app.view.settings.confirm.is_some());
        app.settings_click(SettingsHit::PinModel, false, Instant::now());
        assert!(
            app.view.settings.confirm.is_none(),
            "a click elsewhere says no"
        );
        assert_eq!(app.view.settings.picker, None, "and does nothing else");
        assert_eq!(kind_commands(&app), []);

        press(&mut app, KeyCode::Char('R'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(kind_commands(&app), [Command::ResetKindTiers]);
        assert!(
            app.view
                .settings
                .model
                .kind_tiers
                .iter()
                .all(|r| !r.changed())
        );
    }

    #[test]
    fn reset_all_with_nothing_changed_asks_nothing() {
        let mut app = app_with_review(Tier::High);
        press(&mut app, KeyCode::Char('R'));
        assert!(app.view.settings.confirm.is_none());
    }

    #[test]
    fn the_kind_row_hints_fit_at_100_columns() {
        let mut app = app_with_review(Tier::High);
        app.view.settings.routing_row = review_row();
        let (left, keys) = crate::settings::status_hints(&app);
        assert!(
            left.chars().count() + 2 + keys.chars().count() < 100,
            "{keys}"
        );
    }
}
