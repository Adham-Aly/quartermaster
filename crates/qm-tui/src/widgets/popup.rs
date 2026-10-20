//! A popup window: a rounded, accent-bordered box over the screen beneath, titled in its top
//! border with a ` ✕ ` close button at the top border's right end.
//!
//! Drawing one records, in order: [`PopupHit::Backdrop`] over the whole frame (a click there
//! closes the popup, and nothing beneath lights up or takes the click), [`PopupHit::Window`] over
//! the box (swallows clicks on no control), then [`PopupHit::Close`] over the close button. What
//! the popup holds is drawn after, into the area [`draw`] returns, so its own regions win.

use crate::hit::{FrameMap, Hit};
use crate::text::{cells, truncate, width};
use crate::theme;
use crate::widgets::dropdown::BADGE;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};

/// The close button's label.
const CLOSE: &str = " ✕ ";

/// Something a click on a popup acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PopupHit {
    /// Outside the popup: a click closes it.
    Backdrop,
    /// The popup's box, on no control: swallows the click.
    Window,
    /// The ` ✕ ` in the top border.
    Close,
}

/// What a popup window looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window<'a> {
    pub(crate) title: &'a str,
    /// A red dot after the title.
    pub(crate) badge: bool,
}

impl<'a> Window<'a> {
    pub(crate) const fn new(title: &'a str) -> Self {
        Self {
            title,
            badge: false,
        }
    }

    pub(crate) const fn badge(self, badge: bool) -> Self {
        Self { badge, ..self }
    }
}

/// A box of at most `width` × `height` cells, centred in `within`, leaving a margin around it
/// where `within` has room for one (two columns, one row).
pub(crate) fn centered(within: Rect, width: u16, height: u16) -> Rect {
    let margin_x = if within.width >= 80 { 2 } else { 0 };
    let margin_y = u16::from(within.height >= 20);
    let width = width.min(within.width.saturating_sub(2 * margin_x));
    let height = height.min(within.height.saturating_sub(2 * margin_y));
    Rect::new(
        within.x + (within.width - width) / 2,
        within.y + (within.height - height) / 2,
        width,
        height,
    )
}

/// A box of at most `width` × `height` cells with its left edge at `at.x` and its first row
/// inside the border at `at.y` (its border on the row above), shifted to stay inside `within`.
pub(crate) fn at(within: Rect, at: ratatui::layout::Position, width: u16, height: u16) -> Rect {
    let width = width.min(within.width);
    let height = height.min(within.height);
    let x = at.x.min(within.right().saturating_sub(width)).max(within.x);
    let y =
        at.y.saturating_sub(1)
            .min(within.bottom().saturating_sub(height))
            .max(within.y);
    Rect::new(x, y, width, height)
}

/// Draws `window` at `area` over the frame, clearing what's beneath, and records its regions
/// (see the module's notes). Drops the scroll areas and selectable text recorded so far, so the
/// wheel and selection act only on what the popup holds. Returns the area inside the border.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    window: Window<'_>,
    hover: Option<Hit>,
    map: &mut FrameMap,
) -> Rect {
    map.hits.add(frame.area(), Hit::Popup(PopupHit::Backdrop));
    map.scroll_areas.clear();
    map.text.clear();
    frame.render_widget(Clear, area);
    let close_width = cells(width(CLOSE));
    // `╭─ ` before the title, then ` ` and the badge, and `─ ✕ ─╮` after.
    let room = usize::from(area.width.saturating_sub(close_width + 6));
    let badge_room = if window.badge { 2 } else { 0 };
    let title = truncate(window.title, room.saturating_sub(badge_room));
    let mut spans = vec![
        Span::styled("─", theme::accent()),
        Span::styled(format!(" {title} "), theme::accent()),
    ];
    if window.badge && room >= 2 {
        spans.push(Span::styled(BADGE, theme::badge()));
        spans.push(Span::raw(" "));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Line::from(spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Popup(PopupHit::Window));
    if area.width >= close_width + 4 {
        let close = Rect::new(area.right() - close_width - 2, area.y, close_width, 1);
        let hovered = hover == Some(Hit::Popup(PopupHit::Close));
        let style = if hovered {
            theme::button(true)
        } else {
            theme::accent()
        };
        frame.render_widget(Span::styled(CLOSE, style), close);
        map.hits.add(close, Hit::Popup(PopupHit::Close));
    }
    inner
}
