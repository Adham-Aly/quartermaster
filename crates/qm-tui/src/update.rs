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
use qm_types::api::UpdateView;
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
pub(crate) fn height(view: &UpdateView, room: usize) -> u16 {
    let button = if view.button.is_some() { 2 } else { 0 };
    cells(rows(view, room).len() + button + 2)
}

/// Draws the popup's inside into `area`, and records the button and the selectable text.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let view = &app.snapshot.update;
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
    let Some(pressable) = &view.button else {
        return;
    };
    // A blank row, then the button.
    y += 1;
    if y >= area.bottom() {
        return;
    }
    let hovered = app.term.hover == Some(Hit::UpdateButton);
    frame.render_widget(
        Line::from(button::span(&pressable.label, hovered)),
        Rect::new(area.x, y, area.width, 1),
    );
    let width = button::width_of(&pressable.label).min(area.width);
    map.hits
        .add(Rect::new(area.x, y, width, 1), Hit::UpdateButton);
}

/// The status bar's keys while the popup is on top.
pub(crate) fn hints(app: &App) -> String {
    match &app.snapshot.update.button {
        Some(pressable) => format!("⏎ {} · esc close", pressable.label.to_lowercase()),
        None => "esc close".to_owned(),
    }
}

impl App {
    /// The popup's button: sends the command the core named.
    pub(crate) fn press_update_button(&mut self) {
        if let Some(pressable) = &self.snapshot.update.button {
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
