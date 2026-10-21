//! The screen router: which screen is showing, and who draws it.
//!
//! A new screen is a [`Screen`] variant and a line in [`draw`]. Settings and Help aren't screens:
//! they open as popups over the screen (`popups`). A worker's session isn't one either: it takes
//! the Bridge's middle panel (`view::Middle`).

use crate::app::App;
use crate::bridge;
use crate::hit::FrameMap;
use ratatui::Frame;
use ratatui::layout::Rect;

/// A full screen of the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Screen {
    /// The main layout.
    Bridge,
    /// The setup check: it opens from Settings ▸ System, never by itself.
    Setup,
    /// A held merge's diff: an approval card's Diff opens it.
    Diff,
}

/// Draws the body of the current screen into `body` (the area between the top bar and the status
/// bar), recording what can be clicked (and what scrolls) in `map`.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    match app.screen {
        Screen::Bridge => bridge::draw_body(frame, body, app, map),
        Screen::Setup => crate::setup::draw(frame, body, app, map),
        Screen::Diff => crate::diff::draw(frame, body, app, map),
    }
}
