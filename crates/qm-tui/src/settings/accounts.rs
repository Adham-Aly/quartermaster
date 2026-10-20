//! The Accounts tab: connect, label, pause or disconnect accounts, and mark one preferred for crew
//! work. A Claude account follows your `claude` login, so it is paused here, never disconnected. An
//! account whose login stopped working shows why, with a Reconnect button that replaces its
//! credential in place.

use super::model::{Account, AccountStatus};
use super::paint::Painter;
use super::{SettingsHit, boxed};
use crate::app::App;
use crate::text::{cells, truncate, width, wrap};
use crate::theme;
use ratatui::layout::Rect;

/// Rows per account: its line, its usage line and a blank.
const ROWS_PER_ACCOUNT: u16 = 3;

pub(super) fn draw(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let view = &app.view.settings;
    let accounts = &view.model.accounts;
    // The second line's room: from four cells in (past the dot and name's indent) to one cell
    // inside the right border.
    let line_room = usize::from(area.width.saturating_sub(8)).max(1);
    let rows: u16 = accounts.iter().map(|a| rows_of(a, line_room)).sum::<u16>()
        + if accounts.is_empty() { 3 } else { 1 };
    let inner = boxed(p, area, "Accounts", rows, theme::dim(), theme::dim());
    let x = inner.x + 1;
    let right = inner.right().saturating_sub(1);
    let name_width = accounts
        .iter()
        .map(|a| width(&a.label))
        .max()
        .unwrap_or(0)
        .max(12)
        + 3;
    let mut y = inner.y;
    for (i, account) in accounts.iter().enumerate() {
        let selected = view.account == Some(i);
        let active = account.status == AccountStatus::Active;
        let broken = account.problem.is_some();
        p.hit(
            Rect::new(inner.x, y, inner.width, rows_of(account, line_room) - 1),
            SettingsHit::Account(i),
        );

        // The buttons first, right to left, so the text knows where to stop.
        let left_edge = buttons(p, account, i, right, y);

        let (dot, dot_style) = match (active, broken) {
            (true, true) => ("●", theme::error()),
            (true, false) => ("●", theme::ok()),
            (false, _) => ("○", theme::dim()),
        };
        p.text(x, y, dot, dot_style);
        p.text(x + 2, y, &account.label, theme::strong());
        let detail_x = x + 2 + cells(name_width);
        let mut detail = account.detail.clone();
        if !active {
            detail.push_str(" · paused");
        }
        let room = usize::from(left_edge.saturating_sub(detail_x));
        let detail = truncate(&detail, room);
        let end = p.text(detail_x, y, &detail, theme::dim());
        if account.preferred {
            let star = truncate(
                "★ preferred for crew",
                usize::from(left_edge.saturating_sub(end + 2)),
            );
            p.text(end + 2, y, &star, theme::accent());
        }
        // A broken login says why in place of the usage line, paused or not, wrapped so the
        // whole reason shows.
        if let Some(problem) = account.problem.as_deref() {
            for (row, line) in (y + 1..).zip(wrap(problem, line_room)) {
                p.text(x + 4, row, &line, theme::error());
            }
        } else if active {
            let usage = account.usage.as_deref().unwrap_or("usage unknown");
            p.text(x + 4, y + 1, &truncate(usage, line_room), theme::dim());
        }
        if selected {
            p.select_row(inner, y);
        }
        y += rows_of(account, line_room);
    }
    if accounts.is_empty() {
        p.text(x, y, "No accounts connected yet.", theme::dim());
        y += 2;
    }
    let connect_row = view.account == Some(accounts.len());
    let end = p.button(x, y, "+ Connect an account", SettingsHit::Connect);
    if connect_row {
        p.text(end + 1, y, "◂", theme::accent());
    }
}

/// The rows an account takes at `room` cells of second line: its line, its usage line (or its
/// problem, wrapped) and a blank; a paused, working account has no second line.
fn rows_of(account: &Account, room: usize) -> u16 {
    if let Some(problem) = account.problem.as_deref() {
        ROWS_PER_ACCOUNT - 1 + cells(wrap(problem, room).len().max(1))
    } else if account.status == AccountStatus::Active {
        ROWS_PER_ACCOUNT
    } else {
        ROWS_PER_ACCOUNT - 1
    }
}

/// Draws account `i`'s buttons on row `y`, one cell apart, right to left
/// from `right`; returns where the text must stop.
fn buttons(p: &mut Painter<'_, '_>, account: &Account, i: usize, right: u16, y: u16) -> u16 {
    let active = account.status == AccountStatus::Active;
    let mut left_edge = right;
    if !account.follows_cli {
        left_edge = p.button_left_of(
            left_edge,
            y,
            "Disconnect",
            SettingsHit::AccountDisconnect(i),
        ) - 1;
    }
    let pause = if active { "Pause" } else { "Resume" };
    left_edge = p.button_left_of(left_edge, y, pause, SettingsHit::AccountPause(i)) - 1;
    if active {
        left_edge = p.button_left_of(left_edge, y, "Label", SettingsHit::AccountLabel(i)) - 1;
    }
    if account.problem.is_some() {
        left_edge =
            p.button_left_of(left_edge, y, "Reconnect", SettingsHit::AccountReconnect(i)) - 1;
    }
    left_edge.saturating_sub(1)
}
