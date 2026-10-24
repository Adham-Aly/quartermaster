//! Full-popup rendering and input regressions (including the smallest supported terminal).
use super::*;
use crate::hit::{Hit, Pane};
use crate::popups::Popup;
use crate::widgets::popup::PopupHit;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

const SIZES: [(u16, u16); 5] = [(150, 45), (120, 40), (119, 40), (100, 30), (60, 16)];

fn app(tab: Tab) -> App {
    let mut app = App::new();
    app.view.settings.loaded = true;
    app.view.settings.model = crate::sample::settings();
    app.view.settings.tab = tab;
    app.open_popup(Popup::Settings);
    app
}

fn draw(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    // Selection reveal and resize clamping take effect after a frame.
    terminal.draw(|f| app.draw(f)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now());
}

fn click(app: &mut App, target: SettingsHit) {
    let rect = app.term.drawn.hits.rect_of(Hit::Settings(target)).unwrap();
    assert!(rect.width > 0 && rect.height > 0);
    // Test dispatch through the actual topmost hit, not a direct call to settings_click.
    assert_eq!(
        app.term.drawn.hits.at(rect.x, rect.y),
        Some(Hit::Settings(target))
    );
    app.on_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
        Instant::now(),
    );
}

#[test]
fn popup_grows_by_about_twenty_percent_and_stays_inside_every_terminal() {
    for (width, height) in SIZES.into_iter().chain([(300, 100), (500, 200)]) {
        let mut app = app(Tab::Accounts);
        draw(&mut app, width, height);
        let rect = app
            .term
            .drawn
            .hits
            .rect_of(Hit::Popup(PopupHit::Window))
            .unwrap();
        assert!(rect.right() <= width && rect.bottom() < height);
        if (100..=150).contains(&width) {
            assert_eq!(rect.width, 72);
            assert!((19..=20).contains(&rect.height));
        }
        if width == 300 {
            assert_eq!(rect.width, 144); // was 120
            assert_eq!(rect.height, 47); // was 39
        }
        if width == 500 {
            assert_eq!(rect.width, 149);
        }
    }
}

#[test]
fn routing_scroll_arrows_disappear_at_their_ends_and_when_content_fits() {
    for (width, height) in SIZES {
        let mut app = app(Tab::Routing);
        let screen = draw(&mut app, width, height);
        let area = app
            .term
            .drawn
            .scroll_areas
            .iter()
            .find(|(_, p)| *p == Pane::Settings)
            .unwrap()
            .0;
        let x = usize::from(area.right() - 1);
        let top = usize::from(area.y);
        let bottom = usize::from(area.bottom() - 1);
        let cell = |rows: &[String], y: usize| rows[y].chars().nth(x).unwrap();
        assert_ne!(cell(&screen, top), '↑');
        assert_eq!(cell(&screen, bottom), '↓');
        key(&mut app, KeyCode::PageDown);
        let middle = draw(&mut app, width, height);
        assert_eq!(cell(&middle, top), '↑');
        let max = app.term.drawn.scroll_max_of(Pane::Settings);
        app.view.settings.scroll = max;
        let end = draw(&mut app, width, height);
        assert_eq!(cell(&end, top), '↑');
        assert_ne!(cell(&end, bottom), '↓');
        key(&mut app, KeyCode::Char('5'));
        let system = draw(&mut app, width, height).join("\n");
        assert_eq!(app.term.drawn.scroll_max_of(Pane::Settings), 0);
        assert!(!system.contains('┃'));
    }
}

#[test]
fn both_grid_columns_keep_full_labels_and_open_the_correct_tier_picker() {
    for (width, height) in SIZES {
        let mut app = app(Tab::Routing);
        for (kind, label) in [
            (KindRow::BuildOnFinishedPlan, "Build on a plan"),
            (KindRow::FixOnConfirmedDiagnosis, "Fix on a diagnosis"),
            (KindRow::Fix, "Fix"),
        ] {
            let index = KindRow::ALL.iter().position(|&k| k == kind).unwrap();
            app.view.settings.routing_row = KIND_ROWS_FROM + index;
            app.view.settings.reveal = true;
            let shown = draw(&mut app, width, height).join("\n");
            assert!(shown.contains(label), "{width}×{height}:\n{shown}");
            click(&mut app, SettingsHit::KindTier(kind));
            assert_eq!(
                app.view.settings.picker.as_ref().unwrap().purpose,
                PickFor::KindTier(kind)
            );
            assert!(draw(&mut app, width, height).join("\n").contains("← Back"));
            click(&mut app, SettingsHit::Back);
            assert!(app.view.settings.picker.is_none());
            assert_eq!(app.popup(), Some(Popup::Settings));
        }
    }
}

#[test]
fn catalogue_indicators_and_clickable_back_survive_empty_results_and_resize() {
    let mut app = app(Tab::ModelLevels);
    for (width, height) in SIZES {
        draw(&mut app, width, height);
        key(&mut app, KeyCode::Char('a'));
        let start = draw(&mut app, width, height).join("\n");
        assert!(start.contains("↓ more"));
        assert!(!start.contains("↑ more"));
        key(&mut app, KeyCode::End);
        let end = draw(&mut app, width, height).join("\n");
        assert!(end.contains("↑ more"));
        assert!(!end.contains("↓ more"));
        app.settings_paste("not-a-model");
        let empty = draw(&mut app, width, height).join("\n");
        assert!(empty.contains("No model matches") && empty.contains("← Back"));
        assert!(!empty.contains("more"));
        click(&mut app, SettingsHit::Back);
        assert!(app.view.settings.picker.is_none());
    }
}

#[test]
fn back_and_escape_dismiss_fields_pickers_and_connect_without_saving() {
    for (width, height) in SIZES {
        let mut app = app(Tab::Accounts);
        app.view.settings.model.login = None;
        draw(&mut app, width, height);
        key(&mut app, KeyCode::Char('c'));
        draw(&mut app, width, height);
        click(&mut app, SettingsHit::Back);
        assert!(!app.view.settings.connecting);
        key(&mut app, KeyCode::Char('c'));
        key(&mut app, KeyCode::Esc);
        assert!(!app.view.settings.connecting);
        for purpose in [
            FieldFor::Label(0),
            FieldFor::ApiKey("OpenAI"),
            FieldFor::BaseUrl,
            FieldFor::ReconnectKey(2),
        ] {
            let field = Field {
                purpose,
                text: String::from("unsaved"),
                base_url: None,
            };
            app.view.settings.field = Some(field.clone());
            assert!(draw(&mut app, width, height).join("\n").contains("← Back"));
            click(&mut app, SettingsHit::Back);
            assert!(app.view.settings.field.is_none());
            app.view.settings.field = Some(field);
            key(&mut app, KeyCode::Esc);
            assert!(app.view.settings.field.is_none());
            assert_eq!(app.popup(), Some(Popup::Settings));
        }
        assert_eq!(app.take_commands(), [] as [Command; 0]);
    }
}

#[test]
fn device_code_copy_stays_visible_on_both_sides_of_the_connection_layout_break() {
    for width in [150, 182, 184, 190, 200, 205, 210, 250] {
        let mut app = app(Tab::Accounts);
        app.view.settings.model =
            crate::sample::settings_with(crate::sample::SampleLogin::DeviceCode);
        app.view.settings.connecting = true;
        // Scroll the code into view in a short popup, without changing the live login.
        draw(&mut app, width, 70);
        app.view.settings.scroll = app.term.drawn.scroll_max_of(Pane::Settings);
        let screen = draw(&mut app, width, 70);
        let copy = app
            .term
            .drawn
            .hits
            .rect_of(Hit::Settings(SettingsHit::CopyCode))
            .unwrap();
        assert_eq!(copy.width, crate::widgets::button::width_of("Copy"));
        assert!(screen[usize::from(copy.y)].contains("WDJB-MJHT"));
        assert_eq!(
            app.term.drawn.hits.at(copy.x + 1, copy.y),
            Some(Hit::Settings(SettingsHit::CopyCode))
        );
    }
}
