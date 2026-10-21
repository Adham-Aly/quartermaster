//! Colours and text styles, drawn only from the terminal's own 16-colour palette.
//!
//! Nothing here names an RGB value or a 256-colour index: every colour is one of the 16 the
//! terminal's theme defines, or the terminal's default foreground and background. When the app
//! enters full-screen mode it asks the terminal what those colours are ([`query`]) and fits each
//! role to them ([`fit`]), so text reads in light and dark themes alike. The fitted [`Palette`]
//! belongs to the window's view state and is put in force for each frame with [`scoped`]; the role
//! functions below read it. Where the terminal doesn't say, the roles are the ones tuned for a
//! dark background: dim text and idle borders are bright black, the accent
//! (focus, you, approvals) is yellow, good news is green, a supervisor's marker is bright blue, a
//! supervisor's name magenta and errors are red. Rest buttons, the current tab and the selected
//! row are bright white on bright black.

mod fit;
mod query;

pub(crate) use fit::Palette;
use qm_types::api::Tone;
use ratatui::style::{Color, Modifier, Style};
use std::cell::Cell;

thread_local! {
    /// The palette of the frame being drawn on this thread, set by [`scoped`].
    static CURRENT: Cell<Palette> = const { Cell::new(Palette::DEFAULT) };
}

/// Runs `draw` with `palette` as the palette every role reads, then puts the previous one back
/// (even if `draw` panics).
pub(crate) fn scoped<R>(palette: Palette, draw: impl FnOnce() -> R) -> R {
    struct Restore(Palette);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.set(self.0);
        }
    }
    let _restore = Restore(CURRENT.replace(palette));
    draw()
}

/// The palette in force: the frame's, or [`Palette::DEFAULT`] outside [`scoped`].
fn palette() -> Palette {
    CURRENT.get()
}

/// Asks the terminal for its colours and fits a palette to them. Call it in raw mode, while
/// nothing else reads the terminal's input (which would swallow the answers).
pub(crate) fn detect() -> Palette {
    Palette::fit(&query::ask())
}

/// The background of the fill (rest buttons, the current tab, the selected row).
pub(crate) fn fill_background() -> Color {
    palette().fill
}

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
    Style::new().fg(palette().dim)
}

/// Accent text.
pub(crate) fn accent() -> Style {
    Style::new().fg(palette().accent)
}

/// Good news.
pub(crate) fn ok() -> Style {
    Style::new().fg(palette().ok)
}

/// A warning.
pub(crate) fn warn() -> Style {
    Style::new().fg(palette().accent)
}

/// An error.
pub(crate) fn error() -> Style {
    Style::new().fg(palette().error)
}

/// A supervisor's marker in a session.
pub(crate) fn blue() -> Style {
    Style::new().fg(palette().blue)
}

/// A supervisor's name.
pub(crate) fn violet() -> Style {
    Style::new().fg(palette().violet)
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
/// black in a dark theme.
pub(crate) fn filled() -> Style {
    let p = palette();
    Style::new().fg(p.on_fill).bg(p.fill)
}

/// A run as drawn on the selected row: the fill's background, and the fill's text colour in
/// place of the default and dim foregrounds (dim can't be read on the fill). Coloured glyphs keep
/// their colour where it reads on the fill.
pub(crate) fn on_selection(style: Style) -> Style {
    let p = palette();
    let fg = match style.fg {
        None | Some(Color::Reset) => p.on_fill,
        Some(dim) if dim == p.dim => p.on_fill,
        Some(other) => p.on_fill_glyph(other),
    };
    style.fg(fg).bg(p.fill)
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

/// A rest button on a selected row: the fill's colours inverted, since the selection fill is the
/// rest button's own (black on bright white in a dark theme).
pub(crate) fn button_on_selection() -> Style {
    let p = palette();
    Style::new().fg(p.on_inverse_fill).bg(p.on_fill)
}

/// The cursor's cell in your message: text on the accent (black on yellow in a dark theme).
pub(crate) fn cursor() -> Style {
    on_accent()
}

/// Selected text, in your message as anywhere else: reversed.
pub(crate) fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// A button: the fill at rest, the accent under the pointer (black on yellow in a dark theme).
pub(crate) fn button(hovered: bool) -> Style {
    if hovered { on_accent() } else { filled() }
}

/// Text on the accent; where no accent colour reads in this theme, the fill inverted.
fn on_accent() -> Style {
    let p = palette();
    if p.accent == Color::Reset {
        button_on_selection()
    } else {
        Style::new().fg(p.on_accent).bg(p.accent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fit::{Reported, Rgb};

    #[test]
    fn roles_follow_the_frames_palette() {
        let light = Palette::fit(&Reported {
            bg: Some(Rgb::WHITE),
            ..Reported::default()
        });
        assert_eq!(accent().fg, Some(Color::Yellow));
        scoped(light, || {
            assert_eq!(accent().fg, Some(Color::Blue));
            assert_eq!(warn().fg, Some(Color::Blue));
        });
        // Outside the frame, the default again.
        assert_eq!(accent().fg, Some(Color::Yellow));
    }
}
