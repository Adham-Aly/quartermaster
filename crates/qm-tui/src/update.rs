//! The Update popup, opened from the menu's Update entry or the notice beside the menu button.
//!
//! The core decides every word (`Snapshot::update`): where the update stands, the lines under
//! it, the release's page and the one button ("Update to 0.2.0", "Try again", "Check now"),
//! which sends the command the core named. `⏎` presses it. The core checks for a newer version
//! by itself, so the popup only shows what it found.

use crate::app::App;
use crate::hit::{FrameMap, Hit, TextRow};
use crate::text::{cells, wrap};
use crate::theme;
use crate::widgets::button;
use crossterm::event::KeyCode;
use qm_types::api::{Styled, Tone, UpdatePhase, UpdateView};
use qm_types::wire::ExitReason;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The widest the popup gets, in cells.
pub(crate) const WIDTH: u16 = 84;

/// One row of the popup's text, in its style.
struct Row {
    text: String,
    style: ratatui::style::Style,
    /// It continues the row above after a wrap.
    continues: bool,
}

/// The restart button's label: under a background server an installed update is used by
/// restarting the server, not by reopening the app.
const RESTART: &str = "Restart server";

/// Whether an update is installed and waits for the server to restart.
pub(crate) fn restart_pending(app: &App) -> bool {
    app.detachable && app.snapshot.update.phase == UpdatePhase::Installed
}

/// The core's view, worded for this UI: for a server, the installed line says to restart it.
pub(crate) fn shown(app: &App) -> UpdateView {
    let mut view = app.snapshot.update.clone();
    if restart_pending(app)
        && let Some(version) = &view.available
    {
        view.status.text =
            format!("Quartermaster {version} installed — restart the server to use it.");
        view.notice = Some(Styled::new(
            format!("{version} installed · restart the server"),
            Tone::Ok,
        ));
    }
    view
}

/// The popup's text rows at `room` cells, before the button.
fn rows(view: &UpdateView, room: usize) -> Vec<Row> {
    let mut out = Vec::new();
    let mut push = |text: &str, style| {
        for (i, line) in wrap(text, room).into_iter().enumerate() {
            out.push(Row {
                text: line,
                style,
                continues: i > 0,
            });
        }
    };
    push(&view.status.text, theme::tone(view.status.tone));
    for line in &view.detail {
        push(line, theme::dim());
    }
    if let Some(page) = &view.release_page {
        push(&format!("Release notes: {page}"), theme::dim());
    }
    out
}

/// The popup's height for an inside `room` cells wide: its rows, a blank row and the button,
/// inside the border.
pub(crate) fn height(app: &App, room: usize) -> u16 {
    let view = &shown(app);
    let button = if view.button.is_some() || restart_pending(app) {
        2
    } else {
        0
    };
    cells(rows(view, room).len() + button + 2)
}

/// Draws the popup's inside into `area`, and records the button and the selectable text.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let view = &shown(app);
    let rows = rows(view, usize::from(area.width));
    let mut y = area.y;
    for row in &rows {
        if y >= area.bottom() {
            return;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(row.text.clone(), row.style)),
            Rect::new(area.x, y, area.width, 1),
        );
        map.text.push(TextRow {
            at: Position::new(area.x, y),
            text: row.text.clone(),
            continues: row.continues,
        });
        y += 1;
    }
    let label = if restart_pending(app) {
        RESTART
    } else if let Some(pressable) = &view.button {
        &pressable.label
    } else {
        return;
    };
    // A blank row, then the button.
    y += 1;
    if y >= area.bottom() {
        return;
    }
    let hovered = app.term.hover == Some(Hit::UpdateButton);
    frame.render_widget(
        Line::from(button::span(label, hovered)),
        Rect::new(area.x, y, area.width, 1),
    );
    let width = button::width_of(label).min(area.width);
    map.hits
        .add(Rect::new(area.x, y, width, 1), Hit::UpdateButton);
}

/// The status bar's keys while the popup is on top.
pub(crate) fn hints(app: &App) -> String {
    let label = if restart_pending(app) {
        Some(RESTART)
    } else {
        app.snapshot
            .update
            .button
            .as_ref()
            .map(|b| b.label.as_str())
    };
    match label {
        Some(label) => format!("⏎ {} · esc close", label.to_lowercase()),
        None => "esc close".to_owned(),
    }
}

impl App {
    /// The popup's button: sends the command the core named.
    pub(crate) fn press_update_button(&mut self) {
        if restart_pending(self) {
            self.request_server_stop(ExitReason::Restarting);
        } else if let Some(pressable) = &self.snapshot.update.button {
            let command = pressable.command.clone();
            self.command(command);
        }
    }

    /// The popup's own keys: `⏎` presses its button. `false` for a key it doesn't use.
    pub(crate) fn on_update_key(&mut self, code: KeyCode) -> bool {
        if code == KeyCode::Enter {
            self.press_update_button();
            return true;
        }
        false
    }
}
