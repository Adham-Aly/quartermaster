//! A small painter: text, boxes, buttons and hit regions at cell positions, clipped to an area
//! and scrolled.
//!
//! A tab draws in *page* coordinates: its area starts where the content area starts and runs on
//! downwards for as long as the tab needs. The painter shifts every row up by the scroll offset and
//! clips to the visible area, so a tab taller than the terminal scrolls without knowing it. It also
//! measures how far down the tab drew, and where its selected row is, so the screen can clamp the
//! scroll and keep the selection in view.

use super::SettingsHit;
use crate::hit::{Hit, Hits};
use crate::text::{cells, width};
use crate::theme;
use crate::widgets::button;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};

pub(super) struct Painter<'f, 'a> {
    pub(super) frame: &'f mut Frame<'a>,
    hits: &'f mut Hits,
    hover: Option<Hit>,
    /// Selected rows are filled (the mouse wasn't used last; see `View::pointer_picked`).
    fill_selection: bool,
    /// Nothing is drawn or recorded outside this (screen coordinates).
    clip: Rect,
    /// How many page rows are scrolled off the top: page row `y` shows at `y - scroll`.
    scroll: u16,
    /// Only measuring: nothing is drawn or recorded.
    measuring: bool,
    /// One past the lowest page row drawn so far.
    pub(super) bottom: u16,
    /// The page row of the last selected row drawn.
    pub(super) selected: Option<u16>,
}

impl<'f, 'a> Painter<'f, 'a> {
    pub(super) fn new(
        frame: &'f mut Frame<'a>,
        hits: &'f mut Hits,
        hover: Option<Hit>,
        fill_selection: bool,
        clip: Rect,
    ) -> Self {
        Self {
            frame,
            hits,
            hover,
            fill_selection,
            clip,
            scroll: 0,
            measuring: false,
            bottom: 0,
            selected: None,
        }
    }

    /// Draws from here on into `clip`, scrolled by `scroll` rows; `measuring` draws nothing and
    /// only measures. Resets the measurements.
    pub(super) fn view(&mut self, clip: Rect, scroll: u16, measuring: bool) {
        self.clip = clip;
        self.scroll = scroll;
        self.measuring = measuring;
        self.bottom = 0;
        self.selected = None;
    }

    /// Whether the pointer is over `hit`.
    pub(super) fn hovered(&self, hit: SettingsHit) -> bool {
        self.hover == Some(Hit::Settings(hit))
    }

    /// The screen row of page row `y`, if it shows.
    fn row(&self, y: u16) -> Option<u16> {
        let y = y.checked_sub(self.scroll)?;
        (y >= self.clip.y && y < self.clip.bottom()).then_some(y)
    }

    /// Writes `text` at `(x, y)`, cut at the clip's right edge; returns the column after it.
    pub(super) fn text(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let end = x.saturating_add(cells(width(text))).min(self.clip.right());
        self.bottom = self.bottom.max(y.saturating_add(1));
        if self.measuring || x >= self.clip.right() {
            return end.max(x);
        }
        let Some(row) = self.row(y) else {
            return end.max(x);
        };
        let room = usize::from(self.clip.right() - x);
        let (end, _) = self
            .frame
            .buffer_mut()
            .set_stringn(x, row, text, room, style);
        end
    }

    /// Records `area` (in page rows, clipped) as `hit`.
    pub(super) fn hit(&mut self, area: Rect, hit: SettingsHit) {
        if self.measuring {
            return;
        }
        let top = area.y.saturating_sub(self.scroll);
        let cut = self.scroll.saturating_sub(area.y);
        let shown = Rect::new(area.x, top, area.width, area.height.saturating_sub(cut));
        self.hits
            .add(shown.intersection(self.clip), Hit::Settings(hit));
    }

    /// A button at `(x, y)`; returns the column after it.
    pub(super) fn button(&mut self, x: u16, y: u16, label: &str, hit: SettingsHit) -> u16 {
        let span = button::span(label, self.hovered(hit));
        let end = self.text(x, y, &span.content, span.style);
        self.hit(Rect::new(x, y, end.saturating_sub(x), 1), hit);
        end
    }

    /// A button whose right edge is at `right`; returns its left column.
    pub(super) fn button_left_of(
        &mut self,
        right: u16,
        y: u16,
        label: &str,
        hit: SettingsHit,
    ) -> u16 {
        let x = right.saturating_sub(button::width_of(label));
        self.button(x, y, label, hit);
        x
    }

    /// Text whose right edge is at `right`; returns its left column.
    pub(super) fn text_left_of(&mut self, right: u16, y: u16, text: &str, style: Style) -> u16 {
        let x = right.saturating_sub(cells(width(text)));
        self.text(x, y, text, style);
        x
    }

    /// A rounded box over `rect` (page rows), its title in the top border, drawn cell by cell so
    /// it scrolls with the page.
    pub(super) fn frame_box(&mut self, rect: Rect, title: &str, border: Style, title_style: Style) {
        if rect.width < 2 || rect.height < 2 {
            return;
        }
        let inner = usize::from(rect.width - 2);
        let right = rect.right() - 1;
        let bottom = rect.bottom() - 1;
        let mut top = String::from("╭");
        let mut used = 0;
        if !title.is_empty() && inner >= 3 {
            top.push('─');
            used = 1;
        }
        self.text(rect.x, rect.y, &top, border);
        let mut x = rect.x + cells(top.chars().count());
        if used == 1 {
            let label = crate::text::truncate(&format!(" {title} "), inner - 1);
            x = self.text(x, rect.y, &label, title_style);
            used += width(&label);
        }
        let rest = inner.saturating_sub(used);
        self.text(x, rect.y, &"─".repeat(rest), border);
        self.text(right, rect.y, "╮", border);
        for y in rect.y + 1..bottom {
            self.text(rect.x, y, "│", border);
            self.text(right, y, "│", border);
        }
        self.text(rect.x, bottom, &format!("╰{}╯", "─".repeat(inner)), border);
    }

    /// Blanks `rect` (screen coordinates), for an overlay over the page.
    pub(super) fn clear(&mut self, rect: Rect) {
        if !self.measuring {
            self.frame
                .render_widget(ratatui::widgets::Clear, rect.intersection(self.clip));
        }
    }

    /// Paints page row `y` of `area` as the selected row: the fill's background under every cell
    /// that has none of its own, with dim text brightened so it reads on the fill. A button on
    /// the row would vanish into the fill (both bright black), so it turns black on bright white.
    pub(super) fn select_row(&mut self, area: Rect, y: u16) {
        self.selected = Some(y);
        if self.measuring || !self.fill_selection {
            return;
        }
        let Some(row) = self.row(y) else {
            return;
        };
        let buf = self.frame.buffer_mut();
        for x in area.x..area.right().min(self.clip.right()) {
            if let Some(cell) = buf.cell_mut(Position::new(x, row)) {
                if cell.bg == Color::Reset {
                    let style = theme::on_selection(cell.style());
                    cell.set_style(style);
                } else if cell.bg == theme::DIM {
                    cell.set_style(theme::button_on_selection());
                }
            }
        }
    }
}
