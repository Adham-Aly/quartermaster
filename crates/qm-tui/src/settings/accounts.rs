//! The Accounts tab: connect, label, pause or disconnect accounts, and mark one preferred for crew
//! work. A Claude account follows your `claude` login, so it is paused here, never disconnected. An
//! account whose login stopped working shows why, with a Reconnect button that replaces its
//! credential in place.
//!
//! Each account also shows what routing reads for it: its largest quota pool's percent used, and
//! its soft limit (the `Set`/`Change` button beside it, `s`, or "Soft limit…" on its menu), the
//! percent used past which routing serves its tier from your other accounts first. A note under
//! the list says how that works. An account with no provider quota check says so in a dim note,
//! never as a problem.

use super::model::{Account, AccountStatus};
use super::paint::Painter;
use super::{SettingsHit, section};
use crate::app::App;
use crate::text::{cells, truncate, width, wrap};
use crate::theme;
use ratatui::layout::Rect;
use ratatui::style::Style;

pub(super) fn draw(p: &mut Painter<'_, '_>, area: Rect, app: &App) {
    let view = &app.view.settings;
    let accounts = &view.model.accounts;
    // The second line's room: from four cells in (past the dot and name's indent) to one cell
    // inside the right border.
    let line_room = usize::from(area.width.saturating_sub(8)).max(1);
    let note_room = usize::from(area.width.saturating_sub(3)).max(1);
    let note = wrap(LIMIT_NOTE, note_room);
    let rows: u16 = accounts.iter().map(|a| rows_of(a, line_room)).sum::<u16>()
        + if accounts.is_empty() { 3 } else { 1 }
        + if accounts.is_empty() {
            0
        } else {
            cells(note.len()) + 1
        };
    let inner = section(area, rows);
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
        // Login health, local session usage and provider quotas are independent facts.
        for (row, detail) in (y + 1..).zip(detail_lines(account, line_room)) {
            let end = p.text(x + 4, row, &detail.text, detail.style);
            if let Some(label) = detail.button {
                // Drawn after the row's own hit, so the button takes the click.
                p.button(end + 1, row, label, SettingsHit::AccountSoftLimit(i));
            }
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
    if !accounts.is_empty() {
        for (row, line) in (y + 2..).zip(&note) {
            p.text(x, row, line, theme::dim());
        }
    }
}

/// How quota and soft limits steer routing, under the account list.
const LIMIT_NOTE: &str = "When a tier has several accounts and you name no model, the one with the most \
    quota left serves. A soft limit (s) is a percent USED: past it, the tier's other accounts \
    serve first; if all are past theirs, the one least far past serves. An account with no quota \
    left is never used.";

/// One row under an account's name, and the button drawn one cell after its text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Detail {
    text: String,
    style: Style,
    button: Option<&'static str>,
}

/// The same wrapped lines drive drawing and geometry, including stale data during refresh/errors.
fn detail_lines(account: &Account, room: usize) -> Vec<Detail> {
    let mut lines = Vec::new();
    let add = |lines: &mut Vec<Detail>, text: String, style: Style| {
        lines.extend(wrap(&text, room).into_iter().map(|text| Detail {
            text,
            style,
            button: None,
        }));
    };
    if let Some(problem) = &account.problem {
        add(
            &mut lines,
            format!("Login problem: {problem}"),
            theme::error(),
        );
    }
    if account.status == AccountStatus::Active || account.usage.is_some() {
        let usage = account.usage.as_deref().unwrap_or("unknown");
        add(&mut lines, format!("Session usage: {usage}"), theme::dim());
    }
    if let Some(routing) = &account.routing {
        let held = routing.contains("past its soft limit") || routing.contains("at its limit");
        let style = if held { theme::warn() } else { theme::text() };
        add(&mut lines, format!("Routing: {routing}"), style);
    }
    // At most 21 cells, so never wrapped: the button stays beside the value it changes.
    let (limit, button) = match account.reserve_at {
        Some(mark) => (format!("Soft limit: {mark}% used"), "Change"),
        None => ("Soft limit: none".to_owned(), "Set"),
    };
    lines.push(Detail {
        text: truncate(&limit, room),
        style: theme::text(),
        button: Some(button),
    });
    for line in &account.quota_lines {
        add(&mut lines, format!("Provider quota: {line}"), theme::text());
    }
    if account.quota_refreshing {
        add(
            &mut lines,
            "Provider quota: refreshing…".into(),
            theme::accent(),
        );
    } else if account.quota_lines.is_empty() && account.quota_problem.is_none() {
        // No check at all is a neutral fact, not an unknown reading.
        let note = account.quota_note.as_deref().unwrap_or("unknown");
        add(&mut lines, format!("Provider quota: {note}"), theme::dim());
    }
    if let Some(problem) = &account.quota_problem {
        add(
            &mut lines,
            format!("Quota check problem: {problem}"),
            theme::error(),
        );
    }
    if let Some(checked) = &account.quota_checked_at {
        add(
            &mut lines,
            format!("Quota checked: {checked}"),
            theme::dim(),
        );
    }
    lines
}

/// Header, wrapped details and one blank separator.
fn rows_of(account: &Account, room: usize) -> u16 {
    cells(detail_lines(account, room).len()).saturating_add(2)
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
