//! The busy indicator: one spinning glyph, shown wherever an agent is thinking or working (the
//! conversation while the Quartermaster or a secondmate hasn't written yet, a crewmate's session
//! during its turn, a busy row of the crew tree).
//!
//! It costs nothing while none shows: drawing one marks the frame (`App::spinning`), and only
//! then does `App::tick` advance it, one step every [`STEP`]. Each step changes a single cell, so
//! the terminal gets a few bytes a step. The glyph is the good-news green and the label dim, both
//! from the terminal's own palette, so it reads in light and dark themes alike.

use crate::app::App;
use crate::theme;
use ratatui::text::{Line, Span};
use std::time::Duration;

/// The glyphs, in order: a gap running round a braille block. Dense enough to read in dim
/// light-theme colours; every glyph is one cell wide.
const FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// How long each glyph shows: a full turn a second, eight redraws a second while one is on
/// screen.
pub(crate) const STEP: Duration = Duration::from_millis(125);

/// The step to show `elapsed` after the UI started.
pub(crate) fn step_at(elapsed: Duration) -> usize {
    let step = elapsed.as_millis() / STEP.as_millis();
    // The remainder is below `FRAMES.len()`, so it always fits.
    usize::try_from(step % FRAMES.len() as u128).unwrap_or_default()
}

/// The spinning glyph, in the good-news green; drawing it keeps the animation going.
pub(crate) fn glyph(app: &App) -> Span<'static> {
    app.spinning.set(true);
    Span::styled(FRAMES[app.spin % FRAMES.len()], theme::ok())
}

/// The glyph, a space and `label` dim: `⣾ thinking`.
pub(crate) fn line(app: &App, label: &str) -> Line<'static> {
    Line::from(vec![
        glyph(app),
        Span::raw(" "),
        Span::styled(label.to_owned(), theme::dim()),
    ])
}
