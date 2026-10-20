//! One button: `" label "` in the rest-button colours, or the hover colours under the pointer.
//! Card, task-pane, status-bar and quit-dialog buttons all use it.

use crate::text::{cells, width};
use crate::theme;
use ratatui::text::Span;

/// The button `label`, padded with one space each side.
pub(crate) fn span(label: &str, hovered: bool) -> Span<'static> {
    Span::styled(format!(" {label} "), theme::button(hovered))
}

/// How many cells the button `label` takes, padding included.
pub(crate) fn width_of(label: &str) -> u16 {
    cells(width(label) + 2)
}
