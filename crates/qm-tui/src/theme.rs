//! Colours and text styles, drawn only from the terminal's own 16-colour palette.
//!
//! Nothing here names an RGB value or a 256-colour index: every colour is one of the 16 the
//! terminal's theme defines, or the terminal's default foreground and background, so the app looks
//! right in light and dark themes alike. The roles map as follows: dim text and idle
//! borders are bright black, the accent (focus, you, approvals) is yellow, good news is green, a
//! supervisor's marker is bright blue, a supervisor's name magenta and errors are red. Rest
//! buttons, the current tab and the selected row are bright white on bright black.

use qm_types::api::Tone;
use ratatui::style::{Color, Modifier, Style};

/// Secondary text, idle borders and hints.
pub(crate) const DIM: Color = Color::DarkGray;
/// The accent: the focused pane, your name, approvals, the active input.
pub(crate) const ACCENT: Color = Color::Yellow;
/// Good news: running, ready, passed.
pub(crate) const OK: Color = Color::Green;
/// Warnings such as the setup banner, decisions and notices.
pub(crate) const WARN: Color = Color::Yellow;
/// Blue runs: a session's `[from supervisor]` marker. Bright blue, because ANSI blue on a dark
/// background can hardly be read.
pub(crate) const BLUE: Color = Color::LightBlue;
/// Marked messages from a supervisor.
pub(crate) const VIOLET: Color = Color::Magenta;
/// Errors.
pub(crate) const ERROR: Color = Color::Red;
/// The text on a fill: rest buttons, the current tab, the selected row (bright white).
const ON_FILL: Color = Color::White;

/// The terminal's default text.
pub(crate) fn text() -> Style {
    Style::new()
}

/// A badge: the red dot on a menu entry (and the menu button) that says something there needs
/// you.
pub(crate) fn badge() -> Style {
    error()
}

/// Secondary text.
pub(crate) fn dim() -> Style {
    Style::new().fg(DIM)
}

/// Accent text.
pub(crate) fn accent() -> Style {
    Style::new().fg(ACCENT)
}

/// Good news.
pub(crate) fn ok() -> Style {
    Style::new().fg(OK)
}

/// A warning.
pub(crate) fn warn() -> Style {
    Style::new().fg(WARN)
}

/// An error.
pub(crate) fn error() -> Style {
    Style::new().fg(ERROR)
}

/// A supervisor's marker in a session.
pub(crate) fn blue() -> Style {
    Style::new().fg(BLUE)
}

/// A supervisor's name.
pub(crate) fn violet() -> Style {
    Style::new().fg(VIOLET)
}

/// Bold text in the terminal's default colour (not bright white, which a light theme cannot
/// show).
pub(crate) fn strong() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// A pane border and title: accent when focused, the default colour when the pointer is over it
/// (a click would focus it), dim otherwise.
pub(crate) fn border(focused: bool, hovered: bool) -> Style {
    if focused {
        accent()
    } else if hovered {
        text()
    } else {
        dim()
    }
}

/// The fill of a rest button, the current tab and the selected row: bright white on bright
/// black.
pub(crate) fn filled() -> Style {
    Style::new().fg(ON_FILL).bg(DIM)
}

/// A run as drawn on the selected row: the fill's background, and bright white in place of the
/// default and dim foregrounds (dim can't be read on the fill). Coloured glyphs keep their
/// colour.
pub(crate) fn on_selection(style: Style) -> Style {
    let fg = match style.fg {
        None | Some(DIM | Color::Reset) => ON_FILL,
        Some(other) => other,
    };
    style.fg(fg).bg(DIM)
}

/// The style of a display-ready run from the core.
pub(crate) fn tone(tone: Tone) -> Style {
    match tone {
        Tone::Dim => dim(),
        Tone::Strong => strong(),
        Tone::Accent => accent(),
        Tone::Ok => ok(),
        Tone::Warn => warn(),
        Tone::Error => error(),
        // `Text`, and any tone this build doesn't know.
        _ => text(),
    }
}

/// A rest button on a selected row: black on bright white, since the selection fill is the rest
/// button's own bright black.
pub(crate) fn button_on_selection() -> Style {
    Style::new().fg(Color::Black).bg(ON_FILL)
}

/// A button: bright white on bright black at rest, black on the accent under the pointer.
pub(crate) fn button(hovered: bool) -> Style {
    if hovered {
        Style::new().fg(Color::Black).bg(ACCENT)
    } else {
        filled()
    }
}
