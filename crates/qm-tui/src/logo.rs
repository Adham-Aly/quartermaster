//! The logo: the brand mark drawn as a kitty-protocol image where the terminal shows images,
//! in place of the `◆` the top bar uses everywhere else, and larger on the empty Bridge.
//!
//! The mark (`assets/mark.svg`) is a quarter ring and two squares in the brand teal, so it is
//! drawn here from its geometry, at the window's exact cell size in pixels: sharp at any font
//! size, with nothing to decode.
//!
//! Widgets leave blank cells where the logo goes and [`reserve`] them. After the whole frame is
//! drawn, [`kept`] keeps only the places nothing drew over (a menu, a dialog, a selection), and
//! the host turns those into escapes with [`Shown::update`], after the frame's own bytes: the
//! image is sent once per window, then placed, moved or taken away as the places change.

use base64::Engine as _;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use std::fmt::Write as _;

/// The mark's drawing area (the SVG's `viewBox`).
const VIEW: (f64, f64) = (359.63, 356.07);
/// The ring: a quarter of an annulus around the top-left corner.
const RING: (f64, f64) = (119.63, 239.63);
/// The two squares, as (left, top, side).
const SQUARES: [(f64, f64, f64); 2] = [(239.63, 116.07, 120.0), (118.98, 236.07, 120.0)];
/// The part of the image's height (or width) the mark fills; the rest is an even margin, so the
/// mark never touches the text around it.
const FILL: f64 = 0.84;
/// Samples per pixel along each axis, for smooth edges.
const SAMPLES: u32 = 4;
/// Rows the empty Bridge's logo is tall.
pub(crate) const HERO_ROWS: u16 = 4;
/// Base64 bytes per graphics command, as the protocol asks.
const CHUNK: usize = 4096;

/// Where the logo shows, each its own image (drawn at its own size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Size {
    /// In the top bar's menu button, two cells by one.
    Mark,
    /// On the empty Bridge.
    Hero,
}

impl Size {
    /// The image's id in the terminal: arbitrary, and unlikely to meet another program's.
    const fn id(self) -> u32 {
        match self {
            Self::Mark => 0x51_7a01,
            Self::Hero => 0x51_7a02,
        }
    }

    const ALL: [Self; 2] = [Self::Mark, Self::Hero];
}

/// The mark's colour (the SVG's fill), on every background: it is the brand, not text.
const TEAL: [u8; 3] = [0x25, 0xa4, 0x93];

/// How a window draws the logo: its cell size. A window has one only when its terminal shows
/// images and said how big a cell is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ink {
    /// One cell in pixels, (width, height).
    pub(crate) cell: (u16, u16),
}

impl Ink {
    /// Columns for a logo `rows` tall that keep the mark square on this window's cells.
    pub(crate) fn columns(self, rows: u16) -> u16 {
        let (width, height) = self.cell;
        let columns =
            (u32::from(rows) * u32::from(height) + u32::from(width) / 2) / u32::from(width.max(1));
        u16::try_from(columns.max(1)).unwrap_or(u16::MAX)
    }
}

/// Cells a widget left blank for the logo, and what it drew there.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Spot {
    pub(crate) size: Size,
    pub(crate) area: Rect,
    cells: Vec<Cell>,
}

/// A logo to show this frame: which one, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placed {
    pub(crate) size: Size,
    pub(crate) area: Rect,
}

/// Records `area`, already drawn blank, as the place for logo `size`.
pub(crate) fn reserve(buffer: &Buffer, size: Size, area: Rect) -> Spot {
    Spot {
        size,
        area,
        cells: cells_of(buffer, area),
    }
}

/// The reserved places that still hold what their widget drew: anything drawn over a place
/// since (a menu, a dialog, a selection) wins, and that logo stays hidden this frame.
pub(crate) fn kept(buffer: &Buffer, spots: Vec<Spot>) -> Vec<Placed> {
    spots
        .into_iter()
        .filter(|spot| cells_of(buffer, spot.area) == spot.cells)
        .map(|spot| Placed {
            size: spot.size,
            area: spot.area,
        })
        .collect()
}

fn cells_of(buffer: &Buffer, area: Rect) -> Vec<Cell> {
    let area = area.intersection(buffer.area);
    area.positions().map(|p| buffer[p].clone()).collect()
}

/// What a window's terminal holds of the logo: which images it has and where they are placed.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shown {
    /// Each image sent, with the cells it was drawn for.
    sent: Vec<(Size, (u16, u16))>,
    placed: Vec<Placed>,
}

impl Shown {
    /// Forgets everything: the terminal was cleared, repainted, or has new colours, so the next
    /// update sends and places afresh.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The escapes that bring the terminal from what it shows to `wanted`. Without `ink` the
    /// window shows no logo, so anything placed is taken away.
    pub(crate) fn update(&mut self, ink: Option<Ink>, wanted: &[Placed]) -> Vec<u8> {
        let wanted = if ink.is_some() { wanted } else { &[] };
        let mut out = String::new();
        for gone in self.placed.iter().filter(|p| !wanted.contains(p)) {
            if !wanted.iter().any(|w| w.size == gone.size) {
                let _ = write!(out, "\x1b_Ga=d,d=i,i={},p=1,q=2\x1b\\", gone.size.id());
            }
        }
        if let Some(ink) = ink {
            for want in wanted {
                let cells = (want.area.width, want.area.height);
                if !self.sent.contains(&(want.size, cells)) {
                    out.push_str(&transmit(want.size, ink, cells));
                    self.sent.retain(|(size, _)| *size != want.size);
                    self.sent.push((want.size, cells));
                    // A new image drops its old placement.
                    self.placed.retain(|p| p.size != want.size);
                }
                if !self.placed.contains(want) {
                    out.push_str(&place(*want));
                }
            }
        }
        self.placed = wanted.to_vec();
        out.into_bytes()
    }
}

/// Removes the logo's images from the terminal: written as the app leaves full-screen mode.
pub(crate) fn forget() -> String {
    let mut out = String::new();
    for size in Size::ALL {
        let _ = write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", size.id());
    }
    out
}

/// Sends image `size`, drawn for `cells` (columns, rows), in base64 chunks. `q=2` keeps the
/// terminal from answering, which would arrive as input.
fn transmit(size: Size, ink: Ink, cells: (u16, u16)) -> String {
    let width = u32::from(cells.0) * u32::from(ink.cell.0);
    let height = u32::from(cells.1) * u32::from(ink.cell.1);
    let data = base64::engine::general_purpose::STANDARD.encode(render(width, height));
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    let mut out = String::new();
    for (at, chunk) in chunks.iter().enumerate() {
        let more = u8::from(at + 1 < chunks.len());
        if at == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=t,f=32,s={width},v={height},i={},q=2,m={more};{chunk}\x1b\\",
                size.id()
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
    out
}

/// Places an image over its cells. The cursor is saved and restored around it, and `C=1` keeps
/// the terminal from moving it, so the frame's own cursor stays where it was drawn.
fn place(placed: Placed) -> String {
    let Rect {
        x,
        y,
        width,
        height,
    } = placed.area;
    format!(
        "\x1b7\x1b[{};{}H\x1b_Ga=p,i={},p=1,c={width},r={height},C=1,q=2\x1b\\\x1b8",
        y + 1,
        x + 1,
        placed.size.id()
    )
}

/// The mark as RGBA pixels, `width` × `height`, centred with an even margin, edges smoothed.
fn render(width: u32, height: u32) -> Vec<u8> {
    let scale = (f64::from(width) / VIEW.0).min(f64::from(height) / VIEW.1) * FILL;
    let left = (f64::from(width) - VIEW.0 * scale) / 2.0;
    let top = (f64::from(height) - VIEW.1 * scale) / 2.0;
    let step = 1.0 / f64::from(SAMPLES);
    let total = f64::from(SAMPLES * SAMPLES);
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for py in 0..height {
        for px in 0..width {
            let mut covered = 0_u32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (f64::from(px) + (f64::from(sx) + 0.5) * step - left) / scale;
                    let y = (f64::from(py) + (f64::from(sy) + 0.5) * step - top) / scale;
                    covered += u32::from(inside(x, y));
                }
            }
            let alpha = f64::from(covered) / total * 255.0;
            // In 0..=255 by construction.
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let alpha = alpha.round() as u8;
            out.extend_from_slice(&[TEAL[0], TEAL[1], TEAL[2], alpha]);
        }
    }
    out
}

/// Whether the mark covers the point `(x, y)`, in the mark's own units.
fn inside(x: f64, y: f64) -> bool {
    let ring = x >= 0.0 && y >= 0.0 && (RING.0..=RING.1).contains(&x.hypot(y));
    ring || SQUARES.iter().any(|&(left, top, side)| {
        (left..=left + side).contains(&x) && (top..=top + side).contains(&y)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INK: Ink = Ink { cell: (10, 20) };

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * width + x) * 4) as usize;
        pixels[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn the_mark_is_a_teal_ring_and_two_squares() {
        let pixels = render(200, 200);
        assert_eq!(pixels.len(), 200 * 200 * 4);
        // The mark is 168 pixels (0.84) square, from (16, 16): one mark unit is 168/359.63.
        let at = |x: f64, y: f64| {
            let s = 200.0 * FILL / VIEW.0;
            // In range by construction.
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let p = (
                (x * s + (200.0 - VIEW.0 * s) / 2.0) as u32,
                (y * s + (200.0 - VIEW.1 * s) / 2.0) as u32,
            );
            pixel(&pixels, 200, p.0, p.1)
        };
        // The ring's middle, on its diagonal.
        assert_eq!(at(127.0, 127.0), [0x25, 0xa4, 0x93, 255]);
        // Each square's middle.
        assert_eq!(at(299.0, 176.0), [0x25, 0xa4, 0x93, 255]);
        assert_eq!(at(178.0, 296.0), [0x25, 0xa4, 0x93, 255]);
        // Inside the ring's hole, the empty corners, and the margin.
        assert_eq!(at(40.0, 40.0)[3], 0);
        assert_eq!(at(340.0, 40.0)[3], 0);
        assert_eq!(at(40.0, 340.0)[3], 0);
        assert_eq!(pixel(&pixels, 200, 2, 100)[3], 0);
    }

    #[test]
    fn the_image_is_sent_once_then_only_moved_or_taken_away() {
        let mut shown = Shown::default();
        let mark = Placed {
            size: Size::Mark,
            area: Rect::new(2, 0, 2, 1),
        };
        let first = String::from_utf8(shown.update(Some(INK), &[mark])).unwrap();
        assert!(first.starts_with("\x1b_Ga=t,f=32,s=20,v=20,i=5339649,q=2,m=0;"));
        assert!(
            first.ends_with("\x1b7\x1b[1;3H\x1b_Ga=p,i=5339649,p=1,c=2,r=1,C=1,q=2\x1b\\\x1b8")
        );
        // Nothing changed: nothing to say.
        assert_eq!(shown.update(Some(INK), &[mark]), b"");
        // Covered by a menu: the placement goes, the image stays.
        let hidden = String::from_utf8(shown.update(Some(INK), &[])).unwrap();
        assert_eq!(hidden, "\x1b_Ga=d,d=i,i=5339649,p=1,q=2\x1b\\");
        // Back again: placed, not sent.
        let back = String::from_utf8(shown.update(Some(INK), &[mark])).unwrap();
        assert!(back.starts_with("\x1b7") && !back.contains("a=t"));
        // Moved: placed again where it is now, the old place replaced rather than deleted.
        let moved = Placed {
            area: Rect::new(5, 3, 2, 1),
            ..mark
        };
        let moved = String::from_utf8(shown.update(Some(INK), &[moved])).unwrap();
        assert_eq!(
            moved,
            "\x1b7\x1b[4;6H\x1b_Ga=p,i=5339649,p=1,c=2,r=1,C=1,q=2\x1b\\\x1b8"
        );
        // After a repaint everything is sent afresh.
        shown.reset();
        assert!(
            String::from_utf8(shown.update(Some(INK), &[mark]))
                .unwrap()
                .contains("a=t")
        );
        // A window without images shows none.
        let none = String::from_utf8(shown.update(None, &[mark])).unwrap();
        assert_eq!(none, "\x1b_Ga=d,d=i,i=5339649,p=1,q=2\x1b\\");
    }

    #[test]
    fn a_large_image_goes_in_chunks_that_add_up_to_it() {
        let cells = (9, HERO_ROWS);
        let sent = transmit(Size::Hero, INK, cells);
        let commands: Vec<&str> = sent.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        assert!(commands.len() > 1);
        let mut data = String::new();
        for (at, command) in commands.iter().enumerate() {
            let (keys, chunk) = command
                .trim_start_matches("\x1b_G")
                .split_once(';')
                .unwrap();
            assert!(chunk.len() <= CHUNK);
            let last = at + 1 == commands.len();
            assert!(keys.ends_with(if last { "m=0" } else { "m=1" }), "{keys}");
            assert_eq!(at == 0, keys.starts_with("a=t,f=32,s=90,v=80,"), "{keys}");
            data.push_str(chunk);
        }
        let pixels = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        assert_eq!(pixels, render(90, 80));
    }

    #[test]
    fn a_place_drawn_over_is_not_kept() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let first = reserve(&buffer, Size::Mark, Rect::new(2, 0, 2, 1));
        let second = reserve(&buffer, Size::Hero, Rect::new(5, 0, 3, 2));
        buffer[(6, 1)].set_symbol("x");
        let kept = kept(&buffer, vec![first, second]);
        assert_eq!(
            kept,
            [Placed {
                size: Size::Mark,
                area: Rect::new(2, 0, 2, 1)
            }]
        );
    }

    #[test]
    fn the_hero_keeps_the_mark_square_on_any_cell() {
        assert_eq!(INK.columns(4), 8);
        let tall = Ink { cell: (9, 20) };
        assert_eq!(tall.columns(4), 9);
        assert_eq!(tall.columns(1), 2);
    }
}

#[cfg(test)]
mod app_tests {
    use super::{Ink, Placed, Size};
    use crate::app::App;
    use ratatui::layout::Rect;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(150, 45)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..45)
            .map(|y| (0..150).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn the_logo_takes_the_marks_place_where_the_terminal_shows_images() {
        let mut app = App::new();
        let rows = draw(&mut app);
        assert!(rows[0].starts_with("  ◆ quartermaster ▾ "), "{}", rows[0]);
        assert_eq!(app.term.logos, []);

        app.term.logo = Some(Ink { cell: (10, 20) });
        let rows = draw(&mut app);
        assert!(rows[0].starts_with("     quartermaster ▾ "), "{}", rows[0]);
        assert_eq!(
            app.term.logos[0],
            Placed {
                size: Size::Mark,
                area: Rect::new(2, 0, 2, 1),
            }
        );
        // The empty Bridge shows the large one, square on these cells, over blank cells.
        let hero = app.term.logos[1];
        assert_eq!(
            (hero.size, hero.area.width, hero.area.height),
            (Size::Hero, 8, 4)
        );
        for y in hero.area.top()..hero.area.bottom() {
            let row: String = rows[usize::from(y)]
                .chars()
                .skip(usize::from(hero.area.x))
                .take(8)
                .collect();
            assert_eq!(row, "        ");
        }
        let words = usize::from(hero.area.bottom()) + 1;
        assert!(rows[words].contains("No messages yet."), "{}", rows[words]);
    }
}
