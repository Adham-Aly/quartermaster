//! The Needs-you beacon: the life in the left column's Needs you section while something there is
//! new to you, so you can always tell where to go to answer.
//!
//! ```text
//!  Needs you  2  ───━━━──────
//! ▌ Approve the merge of …
//! ```
//!
//! While an item is unseen, the count's chip breathes (a solid chip, then its usual tint), a
//! lighthouse beam (`━━━`) sweeps along a rule after it, and the newest unseen item's bar pulses
//! (`▏▎▍▌▋▊`). It settles once you have seen them: Needs you takes focus, or you open the item. A
//! new item lights it again. Nothing blinks through the terminal's blink attribute; each step is
//! a redraw, made only while the beacon is on screen (`App::beaconing`), like the busy indicators.

use crate::app::App;
use crate::theme;
use qm_types::api::{ActionItem, ActionKey};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::time::Duration;

/// How long each step of the beacon shows.
pub(crate) const STEP: Duration = Duration::from_millis(90);

/// Steps in one breath of the chip: lit for the first [`BREATH_LIT`].
const BREATH: usize = 18;
const BREATH_LIT: usize = 9;

/// The beam's cells.
const BEAM: usize = 3;
/// Steps the beam rests past the rule's end before it sweeps again.
const BEAM_REST: usize = 8;

/// The bar's pulse, one cell wide, growing and shrinking.
const PULSE: [&str; 10] = ["▏", "▎", "▍", "▌", "▋", "▊", "▋", "▌", "▍", "▎"];

/// The bar every other item shows.
pub(crate) const BAR: &str = "▌";

/// The step to show `elapsed` after the clock it counts from.
pub(crate) fn step_at(elapsed: Duration) -> usize {
    usize::try_from(elapsed.as_millis() / STEP.as_millis()).unwrap_or_default()
}

/// The items you haven't seen yet.
fn unseen(app: &App) -> impl Iterator<Item = &ActionItem> {
    app.snapshot
        .actions
        .iter()
        .filter(|item| !app.view.actions_seen.contains(&item.key))
}

/// Whether the beacon is lit: something waits that you haven't seen. Asking while drawing keeps
/// it moving.
pub(crate) fn lit(app: &App) -> bool {
    let lit = unseen(app).next().is_some();
    if lit {
        app.beaconing.set(true);
    }
    lit
}

/// The newest item you haven't seen, whose bar pulses.
pub(crate) fn newest_unseen(app: &App) -> Option<&ActionKey> {
    unseen(app).max_by_key(|item| item.at).map(|item| &item.key)
}

/// The count's chip, ` 2 `: breathing while lit, else its usual tint.
pub(crate) fn chip(app: &App, count: usize, lit: bool) -> Span<'static> {
    let style = if lit && app.beacon % BREATH < BREATH_LIT {
        theme::warn().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        theme::warn_chip()
    };
    Span::styled(format!(" {count} "), style)
}

/// The rule after the heading, `cells` wide, with the beam sweeping along it.
pub(crate) fn rule(app: &App, cells: usize) -> Vec<Span<'static>> {
    if cells == 0 {
        return Vec::new();
    }
    let at = app.beacon % (cells + BEAM + BEAM_REST);
    let mut spans = Vec::new();
    let mut push = |text: &str, style: Style| match spans.last_mut() {
        Some(Span {
            content,
            style: last,
        }) if *last == style => content.to_mut().push_str(text),
        _ => spans.push(Span::styled(text.to_owned(), style)),
    };
    for cell in 0..cells {
        // The beam's head is at `at`; it trails back `BEAM` cells, brightest at the head.
        match at.checked_sub(cell) {
            Some(0) => push("━", theme::warn().add_modifier(Modifier::BOLD)),
            Some(d) if d < BEAM => push("━", theme::warn()),
            _ => push("─", theme::rule()),
        }
    }
    spans
}

/// An item's bar: pulsing on the newest unseen item, else still.
pub(crate) fn bar(app: &App, item: &ActionItem, style: Style) -> Span<'static> {
    if newest_unseen(app) == Some(&item.key) && lit(app) {
        Span::styled(PULSE[app.beacon % PULSE.len()], style)
    } else {
        Span::styled(BAR, style)
    }
}

impl App {
    /// Marks what you have seen: everything waiting, while Needs you has focus on the Bridge, and
    /// the item whose card is open; and forgets items that are gone, so one that comes back
    /// lights the beacon again.
    pub(crate) fn settle_beacon(&mut self) {
        let keys: Vec<&ActionKey> = self.snapshot.actions.iter().map(|i| &i.key).collect();
        let before = self.view.actions_seen.len();
        self.view.actions_seen.retain(|k| keys.contains(&k));
        let mut seen: Vec<ActionKey> = Vec::new();
        if self.screen == crate::router::Screen::Bridge && self.focus == crate::hit::Pane::Actions {
            seen.extend(keys.iter().map(|k| (*k).clone()));
        }
        if let Some(open) = &self.view.action_open {
            seen.push(open.clone());
        }
        let mut changed = self.view.actions_seen.len() != before;
        for key in seen {
            if !self.view.actions_seen.contains(&key) {
                self.view.actions_seen.push(key);
                changed = true;
            }
        }
        if changed {
            self.redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BEAM, BEAM_REST, rule, step_at};
    use crate::app::App;
    use crate::sample;
    use std::time::Duration;

    fn text(app: &App, cells: usize) -> String {
        rule(app, cells)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn the_beam_sweeps_along_the_rule_then_rests() {
        let mut app = App::new();
        app.beacon = 0;
        assert_eq!(text(&app, 6), "━─────");
        app.beacon = 4;
        assert_eq!(text(&app, 6), "──━━━─");
        app.beacon = 6 + BEAM;
        assert_eq!(text(&app, 6), "──────");
        app.beacon = 6 + BEAM + BEAM_REST;
        assert_eq!(text(&app, 6), "━─────");
        assert_eq!(rule(&app, 0), Vec::new());
        assert_eq!(step_at(Duration::from_millis(270)), 3);
    }

    #[test]
    fn it_settles_once_needs_you_has_focus_and_lights_again_for_a_new_item() {
        let mut app = App::new();
        sample::install(&mut app);
        assert!(super::lit(&app), "the sample's items are new");
        app.focus_pane(crate::hit::Pane::Actions);
        app.settle_beacon();
        assert!(!super::lit(&app));
        // An item that goes and comes back is new again.
        let item = app.snapshot.actions.remove(0);
        app.settle_beacon();
        app.focus_pane(crate::hit::Pane::Input);
        app.snapshot.actions.insert(0, item);
        app.settle_beacon();
        assert!(super::lit(&app));
        assert_eq!(
            super::newest_unseen(&app),
            Some(&app.snapshot.actions[0].key)
        );
    }
}
