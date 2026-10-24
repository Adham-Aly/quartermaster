//! Connecting an account: the subscriptions and API keys on the left, the login in progress on the
//! right. Browser logins open your browser and wait for the callback; device-code logins show the
//! code with a copy button. A Claude subscription isn't logged into here: the app detects your
//! `claude` login.

use super::model::{Login, LoginMethod, LoginStatus, LoginStep};
use super::paint::Painter;
use super::{SettingsHit, SettingsView, boxed};
use crate::app::App;
use crate::text::{cells, truncate, width};
use crate::theme;
use ratatui::layout::Rect;
use std::time::Duration;

/// The spinner's frames, a tenth of a second each.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The hint row.
pub(super) fn hints(view: &SettingsView) -> (&'static str, &'static str) {
    let login = view.model.login.as_ref();
    let right = match login.map(|l| &l.status) {
        Some(LoginStatus::Connected { .. }) => "r review tiers · ⏎ done · esc back",
        Some(LoginStatus::Failed { .. }) => "⏎ try again · x cancel · esc back",
        // Waiting: only a device-code login has a code to copy.
        Some(_) if login.is_some_and(|l| matches!(l.step, LoginStep::DeviceCode { .. })) => {
            "o open browser · y copy code · x cancel · esc back"
        }
        Some(_) => "o open browser · x cancel · esc back",
        None => "↑↓ choose · ⏎ connect · esc back",
    };
    ("click Connect, or choose with the keys", right)
}

/// How long the login's code or link stays good, as of the last tick.
pub(super) fn remaining(login: &Login, view: &SettingsView) -> Option<Duration> {
    let total = login.expires_in?;
    let elapsed = match (login.received, view.now) {
        (Some(at), Some(now)) => now.saturating_duration_since(at),
        _ => Duration::ZERO,
    };
    Some(total.saturating_sub(elapsed))
}

/// The spinner frame for the login, as of the last tick.
pub(super) fn spinner_frame(login: &Login, view: &SettingsView) -> usize {
    let elapsed = match (login.received, view.now) {
        (Some(at), Some(now)) => now.saturating_duration_since(at),
        _ => Duration::ZERO,
    };
    usize::try_from(elapsed.as_millis() / 100).unwrap_or(0) % SPINNER.len()
}

/// The left box's least width: the API-key row (its names, a cell, `Add key`) inside its
/// border and padding, so nothing there is cut.
fn choices_min_width() -> u16 {
    let names = cells(width(&super::model::KEY_PROVIDERS.join(" · ")));
    names + 1 + crate::widgets::button::width_of("Add key") + 4
}

pub(super) fn draw(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let view = &app.view.settings;
    // Keep the login readable in smaller popups by stacking the two boxes.
    if area.width < choices_min_width() + 43 {
        let choices_height = cells(view.model.providers.len()) + 5;
        let choices = Rect::new(area.x, area.y, area.width, choices_height);
        draw_choices(p, choices, view);
        let top = area.y + choices_height + 1;
        draw_login(
            p,
            Rect::new(area.x, top, area.width, area.bottom().saturating_sub(top)),
            view,
        );
        return;
    }
    // Two halves one cell apart (the Bridge's gap), the left never narrower than it needs.
    let half = (area.width.saturating_sub(1) / 2)
        .max(choices_min_width())
        .min(area.width.saturating_sub(1));
    let left = Rect::new(area.x, area.y, half, area.height);
    let right = Rect::new(
        area.x + half + 1,
        area.y,
        area.width.saturating_sub(half + 1),
        area.height,
    );
    draw_choices(p, left, view);
    draw_login(p, right, view);
}

fn draw_choices(p: &mut Painter<'_, '_>, area: Rect, view: &SettingsView) {
    let providers = &view.model.providers;
    // Subscriptions, the providers, API keys and its row: no blank row at the bottom.
    let rows = cells(providers.len()) + 3;
    let inner = boxed(
        p,
        area,
        "Connect an account",
        rows,
        theme::dim(),
        theme::dim(),
    );
    let x = inner.x + 1;
    let right = inner.right().saturating_sub(1);
    // Notes end one cell left of the widest button, so they line up in a column.
    let widest = providers
        .iter()
        .map(|p| crate::widgets::button::width_of(button_of(p).0))
        .max()
        .unwrap_or(0);
    let notes_end = right.saturating_sub(widest + 1);
    // While a login runs, its provider is the selected row.
    let current = view.provider.or_else(|| {
        let login = view.model.login.as_ref()?;
        providers.iter().position(|p| p.id == login.provider)
    });
    let mut y = inner.y;
    p.text(x, y, "Subscriptions", theme::dim());
    y += 1;
    for (i, provider) in providers.iter().enumerate() {
        p.hit(
            Rect::new(inner.x, y, inner.width, 1),
            SettingsHit::Provider(i),
        );
        let (button, method) = button_of(provider);
        let edge = p.button_left_of(right, y, button, SettingsHit::ProviderButton(i));
        let edge = p.text_left_of(
            notes_end.min(edge.saturating_sub(1)),
            y,
            &method,
            theme::dim(),
        );
        let (dot, style) = if provider.connected.is_some() {
            ("●", theme::ok())
        } else {
            ("○", theme::dim())
        };
        p.text(x, y, dot, style);
        let name = truncate(&provider.name, usize::from(edge.saturating_sub(x + 4)));
        p.text(x + 2, y, &name, theme::text());
        if current == Some(i) {
            p.select_row(inner, y);
        }
        y += 1;
    }
    p.text(x, y, "API keys", theme::dim());
    y += 1;
    // The row first, so the button over it takes its own click (later regions win).
    p.hit(
        Rect::new(inner.x, y, inner.width, 1),
        SettingsHit::Provider(providers.len()),
    );
    let edge = p.button_left_of(right, y, "Add key", SettingsHit::AddKey);
    let names = super::model::KEY_PROVIDERS.join(" · ");
    p.text(
        x,
        y,
        &truncate(&names, usize::from(edge.saturating_sub(x + 1))),
        theme::text(),
    );
    if view.provider == Some(providers.len()) {
        p.select_row(inner, y);
    }
}

/// A provider row's button and its note: `Use it` and `logged in · Max`, `Connect` and
/// `browser`.
fn button_of(provider: &super::model::Provider) -> (&'static str, String) {
    match (&provider.connected, provider.method) {
        (Some(how), LoginMethod::Detected) => ("Use it", how.clone()),
        // Signing in again reconnects the account it signs in as.
        (Some(how), _) => ("Sign in", how.clone()),
        (None, LoginMethod::Browser) => ("Connect", String::from("browser")),
        (None, LoginMethod::DeviceCode) => ("Connect", String::from("code")),
        // Detected.
        (None, _) => ("Connect", String::from("log in with claude first")),
    }
}

fn draw_login(p: &mut Painter<'_, '_>, area: Rect, view: &SettingsView) {
    let Some(login) = &view.model.login else {
        let inner = boxed(p, area, "Then", 1, theme::dim(), theme::dim());
        let text = truncate(
            "Choose an account on the left to connect it.",
            usize::from(inner.width.saturating_sub(2)),
        );
        p.text(inner.x + 1, inner.y, &text, theme::dim());
        return;
    };
    let title = format!("Connect {}", login.name);
    let x_of = |inner: Rect| inner.x + 1;
    match &login.status {
        LoginStatus::Connected {
            models,
            starter_tiers,
        } => {
            let inner = boxed(p, area, &title, 3, theme::dim(), theme::dim());
            let x = x_of(inner);
            let end = p.text(x, inner.y, "✓ ", theme::ok());
            p.text(
                end,
                inner.y,
                &format!("Connected: {}", login.name),
                theme::text(),
            );
            let found = if *models == 1 {
                String::from("1 model found")
            } else {
                format!("{models} models found")
            };
            let found = if *starter_tiers {
                format!("{found} · starter tiers filled")
            } else {
                found
            };
            p.text(x + 2, inner.y + 1, &found, theme::dim());
            let end = p.button(x + 2, inner.y + 2, "Review tiers", SettingsHit::ReviewTiers);
            p.button(end + 1, inner.y + 2, "Done", SettingsHit::DoneLogin);
        }
        LoginStatus::Failed { reason } => {
            let inner = boxed(p, area, &title, 3, theme::error(), theme::error());
            let x = x_of(inner);
            let room = usize::from(inner.width.saturating_sub(2));
            p.text(
                x,
                inner.y,
                &truncate(&format!("✗ {reason}"), room),
                theme::error(),
            );
            let end = p.button(x + 2, inner.y + 2, "Try again", SettingsHit::RetryLogin);
            p.button(end + 1, inner.y + 2, "Cancel", SettingsHit::CancelLogin);
        }
        // Waiting.
        _ => draw_waiting(p, area, &title, login, view),
    }
}

fn draw_waiting(
    p: &mut Painter<'_, '_>,
    area: Rect,
    title: &str,
    login: &Login,
    view: &SettingsView,
) {
    let inner = boxed(p, area, title, 6, theme::accent(), theme::accent());
    let x = inner.x + 1;
    let room = usize::from(inner.width.saturating_sub(2));
    let mut y = inner.y;
    match &login.step {
        LoginStep::Browser { url } => {
            let line = format!("1. Sign in at {}", host(url));
            p.text(x, y, &truncate(&line, room), theme::text());
            p.button(x + 3, y + 1, "Open browser", SettingsHit::OpenBrowser);
            let line = "2. Come back here when your browser says you're done";
            p.text(x, y + 2, &truncate(line, room), theme::text());
            y += 3;
        }
        LoginStep::DeviceCode { url, code } => {
            let line = format!("1. Open {}", host(url));
            p.text(x, y, &truncate(&line, room), theme::text());
            p.button(x + 3, y + 1, "Open browser", SettingsHit::OpenBrowser);
            let mut end = p.text(x, y + 2, "2. Enter this code:  ", theme::text());
            end = p.text(end, y + 2, code, theme::strong());
            p.button(end + 2, y + 2, "Copy", SettingsHit::CopyCode);
            y += 3;
        }
        // Starting.
        _ => {
            p.text(x, y, &truncate("Starting the login…", room), theme::dim());
            y += 3;
        }
    }
    y += 1;
    let left = remaining(login, view);
    if left == Some(Duration::ZERO) {
        p.text(x, y, &truncate("The code expired.", room), theme::error());
        let end = p.button(x + 2, y + 1, "Try again", SettingsHit::RetryLogin);
        p.button(end + 1, y + 1, "Cancel", SettingsHit::CancelLogin);
        return;
    }
    let mut end = p.text(x, y, SPINNER[spinner_frame(login, view)], theme::accent());
    end = p.text(
        end + 1,
        y,
        &format!("Waiting for {}…", login.waiting_for),
        theme::text(),
    );
    if let Some(left) = left {
        let secs = left.as_secs();
        p.text(
            end + 1,
            y,
            &format!("expires in {}:{:02}", secs / 60, secs % 60),
            theme::dim(),
        );
    }
    p.button(x + 2, y + 1, "Cancel", SettingsHit::CancelLogin);
}

/// `github.com/login/device` from `https://github.com/login/device`.
fn host(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
}
