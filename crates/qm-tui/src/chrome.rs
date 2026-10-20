//! The frame around every screen: the top bar (the menu button, this Mac, the accounts), the setup
//! banner, the status bar, the quit dialog, and the notice for a terminal too small to draw in.

use crate::app::App;
use crate::hit::{Hit, Hits, StatusItem};
use crate::popups::Popup;
use crate::router::Screen;
use crate::text::{cells, truncate, width};
use crate::theme;
use crate::view::MenuTarget;
use crate::widgets::{button, dropdown};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

/// Below this size the UI shows a notice instead of a squashed Bridge.
pub(crate) const MIN_WIDTH: u16 = 60;
pub(crate) const MIN_HEIGHT: u16 = 16;

/// The status bar's hints on the Bridge.
const BRIDGE_HINTS: &str = "click or tab · ⏎ send · ⌥⏎ newline · ? help";
/// The status bar's hints elsewhere.
const OTHER_HINTS: &str = "esc back";
/// The status bar's hints on Help, which scrolls.
const HELP_HINTS: &str = "↑↓ pgup pgdn scroll · esc close";
/// The menu button's label.
const MENU: &str = "☰ Menu";

/// Draws the top bar, the banner and the status bar, and returns the body area between them, or
/// `None` when the terminal is too small (the notice is drawn instead).
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, hits: &mut Hits) -> Option<Rect> {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return None;
    }
    let mut top = area.y;
    draw_top_bar(frame, Rect::new(area.x, top, area.width, 1), app, hits);
    top += 1;
    // The setup banner is the Bridge's, never over the setup check itself.
    if app.screen == Screen::Bridge && !app.snapshot.setup_issues.is_empty() {
        draw_banner(frame, Rect::new(area.x, top, area.width, 1), app);
        // A click opens the setup check.
        hits.add(
            Rect::new(area.x, top, area.width, 1),
            Hit::Setup(crate::setup::SetupHit::Banner),
        );
        top += 1;
    }
    let status_row = area.bottom() - 1;
    draw_status_bar(
        frame,
        Rect::new(area.x, status_row, area.width, 1),
        app,
        hits,
    );
    Some(Rect::new(area.x, top, area.width, status_row - top))
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let message = format!(
        "Quartermaster needs a terminal of at least {MIN_WIDTH}×{MIN_HEIGHT}; this one is {}×{}.",
        area.width, area.height
    );
    let rows = crate::text::wrap(&message, usize::from(area.width.saturating_sub(2)));
    let height = cells(rows.len()).min(area.height);
    let y = area.y + (area.height - height) / 2;
    let lines: Vec<Line<'_>> = rows.into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(theme::dim()),
        Rect::new(area.x, y, area.width, height),
    );
}

fn draw_top_bar(frame: &mut Frame<'_>, area: Rect, app: &App, hits: &mut Hits) {
    let x = draw_menu_button(frame, area, app, hits);

    // This Mac and one badge per account, right-aligned, never over the tabs.
    let snapshot = &app.snapshot;
    let mut facts: Vec<String> = Vec::new();
    if !snapshot.machine.is_empty() {
        facts.push(snapshot.machine.clone());
    }
    for account in &snapshot.accounts {
        let usage = account.usage.as_deref().unwrap_or("?");
        facts.push(format!("{} {usage}", account.label));
    }
    let start = x + 2;
    let end = area.right().saturating_sub(1);
    if !facts.is_empty() && end > start {
        let room = usize::from(end - start);
        let text = truncate(&facts.join(" · "), room);
        frame.render_widget(
            Paragraph::new(Span::styled(text, theme::dim())).alignment(Alignment::Right),
            Rect::new(start, area.y, end - start, 1),
        );
    }
}

/// The menu button at the top bar's left, ` ☰ Menu `, with a badge when an entry has one; lit
/// while the pointer is over it or its menu is open. Returns the column after it.
fn draw_menu_button(frame: &mut Frame<'_>, area: Rect, app: &App, hits: &mut Hits) -> u16 {
    let open = app
        .view
        .menu
        .as_ref()
        .is_some_and(|menu| menu.target == MenuTarget::Main);
    let lit = open || app.term.hover == Some(Hit::MenuButton);
    let badge = app.menu_badge();
    let style = theme::button(lit);
    let mut spans = vec![Span::raw(" "), Span::styled(format!(" {MENU} "), style)];
    let mut button_width = button::width_of(MENU);
    if badge {
        spans.push(Span::styled(dropdown::BADGE, style.patch(theme::badge())));
        spans.push(Span::styled(" ", style));
        button_width += 2;
    }
    frame.render_widget(Line::from(spans), area);
    let x = area.x + 1;
    hits.add(Rect::new(x, area.y, button_width, 1), Hit::MenuButton);
    x + button_width
}

/// The one-line setup banner: what the setup check found missing.
fn draw_banner(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let issues = app.snapshot.setup_issues.join(" · ");
    let text = truncate(
        &format!("⚠ Setup: {issues}"),
        usize::from(area.width.saturating_sub(2)),
    );
    frame.render_widget(
        Line::from(vec![Span::raw(" "), Span::styled(text, theme::warn())]),
        area,
    );
}

fn draw_status_bar(frame: &mut Frame<'_>, area: Rect, app: &App, hits: &mut Hits) {
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];
    // The quit notice wins over a flash, which replaces the counts.
    let notice = app
        .quit_notice()
        .map(str::to_owned)
        .or_else(|| app.term.flash.as_ref().map(|f| f.text.clone()));
    if let Some(notice) = notice {
        left.push(Span::styled(notice, theme::accent()));
    } else if app.popup() == Some(Popup::Settings) {
        // The tab's own hint row: what the mouse does.
        left.push(Span::styled(
            crate::settings::status_hints(app).0,
            theme::dim(),
        ));
    } else if app.screen == Screen::Setup {
        if let Some(text) = crate::setup::status_left(app) {
            left.push(Span::styled(text, theme::dim()));
        }
    } else {
        status_counts(&mut left, area, app, hits);
    }
    let left = Line::from(left);
    let left_width = cells(left.width());
    frame.render_widget(left, area);

    // The popup's or the screen's keys on the right: one bottom row, never a second hint row.
    let hints: String = match (app.popup(), app.screen) {
        (Some(Popup::Settings), _) => crate::settings::status_hints(app).1.to_owned(),
        (Some(Popup::Help), _) => HELP_HINTS.to_owned(),
        (None, Screen::Bridge) => BRIDGE_HINTS.to_owned(),
        (None, Screen::Setup) => crate::setup::hints(app),
        (None, Screen::Diff) => crate::diff::HINTS.to_owned(),
        (None, Screen::Session(_)) => OTHER_HINTS.to_owned(),
    };
    let hints = hints.as_str();
    let hints_width = cells(width(hints)) + 1;
    if left_width + 2 + hints_width <= area.width {
        frame.render_widget(
            Paragraph::new(Span::styled(hints, theme::dim())).alignment(Alignment::Right),
            Rect::new(
                area.right() - hints_width,
                area.y,
                hints_width.saturating_sub(1),
                1,
            ),
        );
    }
}

/// `⚑ 1 approval` and `⚑ 1 decision` as buttons (each left out at zero), then `· 4 running`.
fn status_counts(left: &mut Vec<Span<'static>>, area: Rect, app: &App, hits: &mut Hits) {
    let attention = app.snapshot.attention;
    let counts = [
        (
            attention.approvals,
            "approval",
            "approvals",
            StatusItem::Approvals,
        ),
        (
            attention.decisions,
            "decision",
            "decisions",
            StatusItem::Decisions,
        ),
    ];
    let mut x = area.x + cells(left.iter().map(|s| width(&s.content)).sum());
    let mut any = false;
    for (n, one, many, item) in counts {
        if n == 0 {
            continue;
        }
        if any {
            left.push(Span::raw(" "));
            x += 1;
        }
        any = true;
        let label = counted(n, one, many);
        let hit = Hit::Status(item);
        let w = button::width_of(&label);
        left.push(button::span(&label, app.term.hover == Some(hit)));
        hits.add(Rect::new(x, area.y, w, 1), hit);
        x += w;
    }
    if any {
        left.push(Span::styled(" · ", theme::dim()));
        x += 3;
    }
    if attention.running > 0 {
        let running = format!("{} running", attention.running);
        let hit = Hit::Status(StatusItem::Running);
        let style = if app.term.hover == Some(hit) {
            theme::dim().add_modifier(Modifier::UNDERLINED)
        } else {
            theme::dim()
        };
        hits.add(Rect::new(x, area.y, cells(width(&running)), 1), hit);
        left.push(Span::styled(running, style));
    } else {
        left.push(Span::styled("nothing running", theme::dim()));
    }
}

fn counted(n: u32, one: &str, many: &str) -> String {
    format!("⚑ {n} {}", if n == 1 { one } else { many })
}

/// The dialog shown when the core says agents are working.
pub(crate) fn draw_quit_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    working: u32,
    hover: Option<Hit>,
    hits: &mut Hits,
) {
    let message = if working == 1 {
        "1 crewmate is working; it'll pick up again next launch.".to_owned()
    } else {
        format!("{working} crewmates are working; they'll pick up again next launch.")
    };
    let dialog_width = area.width.saturating_sub(4).min(58);
    let inner_width = dialog_width.saturating_sub(4);
    let rows = crate::text::wrap(&message, usize::from(inner_width));
    let dialog_height = (cells(rows.len()) + 5).min(area.height);
    let dialog = Rect::new(
        area.x + (area.width - dialog_width) / 2,
        area.y + (area.height - dialog_height) / 2,
        dialog_width,
        dialog_height,
    );
    frame.render_widget(Clear, dialog);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Line::from(vec![
            Span::styled("─", theme::accent()),
            Span::styled(" Quit? ", theme::accent()),
        ]));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    hits.add(dialog, Hit::Dialog);

    let mut lines: Vec<Line<'_>> = rows.into_iter().map(Line::from).collect();
    lines.push(Line::default());
    let quit = "Quit";
    let cancel = "Cancel";
    lines.push(Line::from(vec![
        button::span(quit, hover == Some(Hit::ConfirmQuit)),
        Span::raw(" "),
        button::span(cancel, hover == Some(Hit::CancelQuit)),
        Span::styled("   ⏎ quit · esc cancel", theme::dim()),
    ]));
    let text_area = Rect::new(
        inner.x + 1,
        inner.y + 1,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(1),
    );
    let buttons_row = text_area.y + cells(lines.len()) - 1;
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text_area);
    if buttons_row < inner.bottom() {
        let quit_width = button::width_of(quit);
        hits.add(
            Rect::new(text_area.x, buttons_row, quit_width, 1),
            Hit::ConfirmQuit,
        );
        hits.add(
            Rect::new(
                text_area.x + quit_width + 1,
                buttons_row,
                button::width_of(cancel),
                1,
            ),
            Hit::CancelQuit,
        );
    }
}
