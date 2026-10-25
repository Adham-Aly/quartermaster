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

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::ids::AccountId;

    #[test]
    fn quota_details_preserve_login_errors_usage_and_cached_results() {
        let mut account = Account::new(AccountId::new(1), "Work");
        account.problem = Some("login expired; reconnect".into());
        account.usage = Some("12 turns · 42k tokens".into());
        account.quota_lines = vec!["5-hour window: 41% used · resets today 14:30".into()];
        account.quota_problem = Some("provider temporarily unavailable".into());
        account.quota_refreshing = true;
        account.quota_checked_at = Some("today 12:04".into());
        for room in [92, 111, 112, 142] {
            let lines = detail_lines(&account, room);
            let text = lines
                .iter()
                .map(|d| d.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            for expected in [
                "Login problem:",
                "12 turns",
                "41% used",
                "refreshing",
                "Quota check problem:",
                "today 12:04",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(lines.iter().all(|d| width(&d.text) <= room));
            assert_eq!(usize::from(rows_of(&account, room)), lines.len() + 2);
        }
        account.status = AccountStatus::Paused;
        assert!(
            detail_lines(&account, 92)
                .iter()
                .any(|d| d.text.contains("Session usage:"))
        );
    }

    #[test]
    fn accounts_render_quota_details_at_supported_sizes_in_both_palettes() {
        use crate::popups::Popup;
        use crate::settings::Tab;
        use ratatui::{Terminal, backend::TestBackend};

        for (w, h) in [(150, 45), (120, 40), (119, 40), (100, 30)] {
            for light in [false, true] {
                let mut app = App::new();
                app.open_popup(Popup::Settings);
                app.view.settings.loaded = true;
                app.view.settings.tab = Tab::Accounts;
                let mut account = Account::new(AccountId::new(1), "Work");
                account.problem = Some("login expired; reconnect to continue".into());
                account.usage = Some("12 turns · 42k tokens".into());
                account.quota_lines = vec!["5-hour window: 41% used · resets today 14:30".into()];
                account.quota_problem = Some("provider temporarily unavailable".into());
                account.quota_refreshing = true;
                account.quota_checked_at = Some("today 12:04".into());
                account.routing =
                    Some("7-day window 60% used · past its soft limit: others serve first".into());
                account.reserve_at = Some(55);
                let mut view = qm_types::api::SettingsView::default();
                view.accounts.push(account);
                app.view.settings.model = super::super::model::Settings::from_view(&view);
                app.term.palette = theme::fit_reported(&qm_types::wire::Colors {
                    bg: Some(if light { [255; 3] } else { [0; 3] }),
                    ..Default::default()
                });
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                // The note under the list is the tab's last row; scroll it into view.
                app.view.settings.scroll = app.term.drawn.scroll_max_of(crate::hit::Pane::Settings);
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let top = (0..h)
                    .map(|y| {
                        (0..w)
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    top.contains("never used."),
                    "the note is cut off at {w}x{h}:\n{top}"
                );
                app.view.settings.scroll = 0;
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let buffer = terminal.backend().buffer();
                let text = (0..h)
                    .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                for expected in [
                    "Login problem:",
                    "12 turns",
                    "41% used",
                    "refreshing",
                    "Quota check problem:",
                    "today 12:04",
                    "Routing: 7-day window 60% used",
                    "Soft limit: 55% used  Change ",
                    "+ Connect an account",
                    "When a tier has several accounts",
                ] {
                    assert!(
                        text.contains(expected),
                        "missing {expected} at {w}x{h}:\n{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_quota_is_not_zero_and_long_details_wrap() {
        let mut account = Account::new(AccountId::new(1), "Work");
        assert!(
            detail_lines(&account, 100)
                .iter()
                .any(|d| d.text == "Provider quota: unknown")
        );
        account.quota_problem =
            Some("a long provider error that should wrap without truncation".into());
        let lines = detail_lines(&account, 20);
        assert!(lines.len() > 3);
        assert!(lines.iter().all(|d| width(&d.text) <= 20));
        assert_eq!(usize::from(rows_of(&account, 20)), lines.len() + 2);
    }

    #[test]
    fn the_routing_line_warns_when_held_back_and_wraps() {
        let mut account = Account::new(AccountId::new(1), "Work");
        account.routing = Some("7-day window 30% used".into());
        let routing = |a: &Account, room| {
            detail_lines(a, room)
                .into_iter()
                .filter(|d| {
                    !d.text.starts_with("Session usage")
                        && !d.text.starts_with("Provider quota")
                        && !d.text.starts_with("Soft limit")
                })
                .map(|d| (d.text, d.style))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            routing(&account, 100),
            [("Routing: 7-day window 30% used".to_owned(), theme::text())]
        );
        for state in [
            "past its soft limit: others serve first",
            "at its limit: skipped until it resets",
        ] {
            account.routing = Some(format!("7-day window 90% used · {state}"));
            let lines = routing(&account, 30);
            assert!(lines.len() > 1);
            assert!(
                lines
                    .iter()
                    .all(|(line, style)| width(line) <= 30 && *style == theme::warn())
            );
        }
    }

    #[test]
    fn the_soft_limit_line_sits_under_routing_with_its_button() {
        let mut account = Account::new(AccountId::new(1), "Work");
        account.routing = Some("7-day window 30% used".into());
        let lines = detail_lines(&account, 100);
        let at = lines
            .iter()
            .position(|d| d.text.starts_with("Routing:"))
            .unwrap();
        assert_eq!(
            lines[at + 1],
            Detail {
                text: "Soft limit: none".into(),
                style: theme::text(),
                button: Some("Set"),
            }
        );
        account.reserve_at = Some(55);
        account.status = AccountStatus::Paused;
        let lines = detail_lines(&account, 100);
        let limit = lines
            .iter()
            .find(|d| d.text.starts_with("Soft limit"))
            .unwrap();
        assert_eq!(limit.text, "Soft limit: 55% used");
        assert_eq!(limit.button, Some("Change"));
        // The only line with a button.
        assert_eq!(lines.iter().filter(|d| d.button.is_some()).count(), 1);
    }

    #[test]
    fn a_quota_note_is_dim_and_replaces_unknown() {
        let mut account = Account::new(AccountId::new(1), "Claude Max");
        account.quota_note = Some("not offered; Claude reports its usage during turns".into());
        let lines = detail_lines(&account, 100);
        let quota = lines
            .iter()
            .filter(|d| d.text.starts_with("Provider quota"))
            .collect::<Vec<_>>();
        assert_eq!(quota.len(), 1);
        assert_eq!(
            quota[0].text,
            "Provider quota: not offered; Claude reports its usage during turns"
        );
        assert_eq!(quota[0].style, theme::dim());
        for absent in [
            "Provider quota: unknown",
            "Quota check problem",
            "refreshing",
        ] {
            assert!(
                !lines.iter().any(|d| d.text.contains(absent)),
                "{absent}: {lines:?}"
            );
        }
    }
}
