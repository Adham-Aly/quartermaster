//! The Settings overlays: a picker (an effort, a model, where to move a model, find) under the
//! control that opened it, and a one-line text field (an account's label, an API key).

use super::paint::Painter;
use super::{Field, FieldFor, Picker, SettingsHit, boxed};
use crate::app::App;
use crate::text::{cells, truncate, width};
use crate::theme;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

/// The most rows a picker shows at once.
const PICKER_ROWS: usize = 12;

pub(super) fn draw(p: &mut Painter<'_, '_>, body: Rect, app: &App) {
    let view = &app.view.settings;
    if view.picker.is_some() || view.field.is_some() {
        p.hit(body, SettingsHit::Backdrop);
    }
    if let Some(picker) = &view.picker {
        draw_picker(p, body, picker);
    }
    if let Some(field) = &view.field {
        draw_field(p, body, field, app);
    }
}

fn draw_picker(p: &mut Painter<'_, '_>, body: Rect, picker: &Picker) {
    let visible = picker.visible();
    let widest = picker
        .options
        .iter()
        .map(|(label, _)| width(label))
        .max()
        .unwrap_or(0)
        .max(24);
    let find = picker.query.is_some();
    let shown = visible.len().clamp(1, PICKER_ROWS);
    let box_width = (cells(widest) + 4).min(body.width);
    let x = picker
        .under
        .x
        .min(body.right().saturating_sub(box_width))
        .max(body.x);
    // Under its control, or over it when there's more room there; never away from it.
    let below = picker.under.bottom();
    let room_below = body.bottom().saturating_sub(below);
    let room_above = picker.under.y.saturating_sub(body.y);
    let wanted = cells(shown) + u16::from(find) + 2;
    let (y, room) = if wanted <= room_below || room_below >= room_above {
        (below, room_below)
    } else {
        let height = wanted.min(room_above);
        (picker.under.y - height, room_above)
    };
    let box_height = wanted.min(room).max(3);
    let shown = usize::from(box_height - 2 - u16::from(find)).max(1);
    let rows = cells(shown) + u16::from(find);
    let area = Rect::new(x, y, box_width, box_height);
    p.clear(area);
    p.hit(area, SettingsHit::Overlay);
    let inner = boxed(p, area, "", rows, theme::accent(), theme::accent());
    let room = usize::from(inner.width.saturating_sub(1));
    let mut row = inner.y;
    if let Some(query) = &picker.query {
        let line = format!(" find: {query}▏");
        p.text(inner.x, row, &truncate(&line, room), theme::accent());
        row += 1;
    }
    if visible.is_empty() {
        p.text(inner.x, row, " No model matches", theme::dim());
        return;
    }
    let cursor_at = visible
        .iter()
        .position(|&i| i == picker.cursor)
        .unwrap_or(0);
    let first = cursor_at.saturating_sub(shown - 1);
    for &i in visible.iter().skip(first).take(shown) {
        let hit = SettingsHit::PickerItem(i);
        let label = truncate(&format!(" {}", picker.options[i].0), room);
        let pad = usize::from(inner.width).saturating_sub(width(&label));
        let style = if i == picker.cursor || p.hovered(hit) {
            theme::text().add_modifier(Modifier::REVERSED)
        } else {
            theme::text()
        };
        p.text(inner.x, row, &format!("{label}{}", " ".repeat(pad)), style);
        p.hit(Rect::new(inner.x, row, inner.width, 1), hit);
        row += 1;
    }
}

fn draw_field(p: &mut Painter<'_, '_>, body: Rect, field: &Field, app: &App) {
    let view = &app.view.settings;
    let box_width = body.width.min(64);
    let box_height = 6u16.min(body.height);
    let area = Rect::new(
        body.x + (body.width - box_width) / 2,
        body.y + body.height.saturating_sub(box_height) / 3,
        box_width,
        box_height,
    );
    p.clear(area);
    p.hit(area, SettingsHit::Overlay);
    let (title, shown, note) = match field.purpose {
        FieldFor::Label(i) => {
            let name = view
                .model
                .accounts
                .get(i)
                .map_or("the account", |a| a.label.as_str());
            (format!("Label {name}"), field.text.clone(), "")
        }
        FieldFor::BaseUrl => (
            String::from("Base URL for a custom endpoint"),
            field.text.clone(),
            "OpenAI-compatible, such as https://llm.example.com/v1",
        ),
        FieldFor::ApiKey(provider) => (
            format!("Add {provider} key"),
            "•".repeat(field.text.chars().count()),
            "Stored in Quartermaster's credential store.",
        ),
        FieldFor::ReconnectKey(i) => {
            let name = view
                .model
                .accounts
                .get(i)
                .map_or("the account", |a| a.label.as_str());
            (
                format!("New key for {name}"),
                "•".repeat(field.text.chars().count()),
                "Replaces its old key; the account keeps its tiers.",
            )
        }
    };
    let inner = boxed(p, area, &title, 4, theme::accent(), theme::accent());
    let room = usize::from(inner.width.saturating_sub(2));
    let end = p.text(inner.x + 1, inner.y, "› ", theme::accent());
    let line = format!("{}▏", crate::text::tail(&shown, room.saturating_sub(3)));
    p.text(end, inner.y, &line, theme::text());
    p.text(
        inner.x + 1,
        inner.y + 1,
        &truncate(note, room),
        theme::dim(),
    );
    let end = p.button(inner.x + 1, inner.y + 3, "Save", SettingsHit::FieldSave);
    p.button(end + 1, inner.y + 3, "Cancel", SettingsHit::FieldCancel);
}
