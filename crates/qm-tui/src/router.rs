//! The screen router: which screen is showing, and who draws it.
//!
//! A new screen is a [`Screen`] variant, its tab label in [`Screen::TABS`] if it is a top-bar tab,
//! and a line in [`draw`].

use crate::app::App;
use crate::hit::FrameMap;
use crate::{bridge, placeholder, settings};
use ratatui::Frame;
use ratatui::layout::Rect;

/// A full screen of the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen {
    /// The main layout.
    Bridge,
    /// Settings and accounts.
    Settings,
    /// Keys and mouse help.
    Help,
    /// The setup check: not a tab; it opens by itself, and from the banner.
    Setup,
    /// A worker's session: not a tab; opened from the crew. It owns the whole frame, with no top
    /// bar or status bar.
    Session(qm_types::api::AgentRef),
    /// A held merge's diff: not a tab; an approval card's Diff opens it.
    Diff,
}

impl Screen {
    /// The top bar's tabs, left to right.
    pub(crate) const TABS: [Self; 3] = [Self::Bridge, Self::Settings, Self::Help];

    /// The tab's label.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Bridge => "Bridge",
            Self::Settings => "Settings",
            Self::Help => "Help",
            Self::Setup => "Setup check",
            Self::Session(_) => "Session",
            Self::Diff => "Diff",
        }
    }
}

/// Draws the body of the current screen into `body` (the area between the top bar, or the setup
/// banner, and the status bar), recording what can be clicked (and what scrolls) in `map`.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    match app.screen {
        Screen::Bridge => bridge::draw_body(frame, body, app, map),
        Screen::Settings => settings::draw(frame, body, app, map),
        Screen::Help => placeholder::draw_help(frame, body, app, map),
        Screen::Setup => crate::setup::draw(frame, body, app, map),
        Screen::Session(agent) => crate::session::draw(frame, body, app, agent, map),
        Screen::Diff => crate::diff::draw(frame, body, app, map),
    }
}
