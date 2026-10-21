//! The Routing tab: the pin and the Quartermaster's own model and effort. There is no switch to
//! turn routing off; while a pin is on, a notice says tiers are being skipped.

use super::model::{QuartermasterModel, Settings, effort_label};
use super::paint::Painter;
use super::{SettingsHit, boxed, paragraph, section};
use crate::app::App;
use crate::text::truncate;
use crate::theme;
use qm_types::config::AutocompactWindow;
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

    // Autocompact, then Decisions below it.
    autocompact(p, inner, col, inner.y + 10, settings, view.routing_row == 2);

    // Decisions, not in this version.
    let y = inner.y + 15;
    p.text(x, y, "Decisions", theme::dim().add_modifier(Modifier::BOLD));
    let left = p.text_left_of(right, y, "not in this version", theme::dim());
    let text = truncate(
        "later: a small model that decides task kinds",
        usize::from(left.saturating_sub(col + 1)),
    );
    p.text(col, y, &text, theme::dim());

    draw_notice(p, area, inner, settings);
}

/// The Autocompact help, under its arrows: what it applies to and when a change lands.
const AUTOCOMPACT_COPY: [&str; 3] = [
    "Every agent, on any model, compacts its conversation a little before",
    "this size, or before its model's window if that is smaller.",
    "Changes apply at each agent's next idle moment.",
];

/// The Autocompact block at `y`: its arrows and value, then what it applies to. Selected when
/// `selected`.
fn autocompact(
    p: &mut Painter<'_, '_>,
    inner: Rect,
    col: u16,
    y: u16,
    settings: &Settings,
    selected: bool,
) {
    p.text(inner.x + 1, y, "Autocompact", theme::strong());
    autocompact_arrows(p, col, y, settings.autocompact_window);
    for (i, line) in AUTOCOMPACT_COPY.iter().enumerate() {
        let row = y + 1 + u16::try_from(i).unwrap_or(0);
        p.text(col, row, line, theme::dim());
    }
    if selected {
        p.select_row(inner, y);
    }
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
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.view.settings.routing_row, 2, "stops at the last row");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.view.settings.routing_row, 1);
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
    fn the_copy_fits_at_100_columns() {
        let (buffer, _) = render(&app_at(200_000), 100, 30, false);
        let screen = rows(&buffer);
        for line in AUTOCOMPACT_COPY {
            assert!(
                screen.iter().any(|row| row.contains(line)),
                "{line:?} is cut off at 100 columns:\n{}",
                screen.join("\n")
            );
        }
        assert!(screen.iter().any(|row| row.contains("◂  200k  ▸")));
        assert!(screen.iter().any(|row| row.contains("Autocompact")));
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
}
