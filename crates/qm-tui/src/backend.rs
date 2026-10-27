//! The terminal backend: crossterm's, except that named colours go out as the 16-colour SGR codes
//! (`30`–`37`, `90`–`97`, `40`–`47`, `100`–`107`).
//!
//! crossterm writes even the 16 named colours in 256-colour form (`ESC[38;5;15m` for white). The
//! app never uses a 256-colour value: in 16-colour mode a terminal maps every colour through its
//! own palette, and the 24-bit palette (only with `COLORTERM`) goes out as `38;2`/`48;2`. This
//! backend draws cells itself and hands everything else to [`CrosstermBackend`].

use crossterm::cursor::MoveTo;
use crossterm::queue;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use std::fmt::Write as _;
use std::io::{self, Write};

/// [`CrosstermBackend`] with 16-colour SGR codes.
#[derive(Debug)]
pub(crate) struct Backend16<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> Backend16<W> {
    pub(crate) const fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
        }
    }
}

impl<W: Write> Write for Backend16<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

/// The SGR sequence that sets exactly `modifier`, `fg` and `bg`, from a reset.
pub(crate) fn sgr(modifier: Modifier, fg: Color, bg: Color) -> String {
    let mut out = String::from("\x1b[0");
    for (flag, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::SLOW_BLINK, 5),
        (Modifier::RAPID_BLINK, 6),
        (Modifier::REVERSED, 7),
        (Modifier::HIDDEN, 8),
        (Modifier::CROSSED_OUT, 9),
    ] {
        if modifier.contains(flag) {
            let _ = write!(out, ";{code}");
        }
    }
    push_color(&mut out, fg, 30);
    push_color(&mut out, bg, 40);
    out.push('m');
    out
}

/// Appends `color` as a foreground (`base` 30) or background (`base` 40) parameter. The 16 named
/// colours use their own codes; anything else (which the app never uses) is written as crossterm
/// would.
fn push_color(out: &mut String, color: Color, base: u8) {
    let code = match color {
        Color::Reset => return,
        Color::Black => base,
        Color::Red => base + 1,
        Color::Green => base + 2,
        Color::Yellow => base + 3,
        Color::Blue => base + 4,
        Color::Magenta => base + 5,
        Color::Cyan => base + 6,
        Color::Gray => base + 7,
        Color::DarkGray => base + 60,
        Color::LightRed => base + 61,
        Color::LightGreen => base + 62,
        Color::LightYellow => base + 63,
        Color::LightBlue => base + 64,
        Color::LightMagenta => base + 65,
        Color::LightCyan => base + 66,
        Color::White => base + 67,
        Color::Indexed(i) => {
            let _ = write!(out, ";{};5;{i}", base + 8);
            return;
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{};2;{r};{g};{b}", base + 8);
            return;
        }
    };
    let _ = write!(out, ";{code}");
}

impl<W: Write> Backend for Backend16<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut style = (Modifier::empty(), Color::Reset, Color::Reset);
        let mut last: Option<Position> = None;
        for (x, y, cell) in content {
            // Move only when this cell doesn't follow the last one.
            if !matches!(last, Some(p) if x == p.x + 1 && y == p.y) {
                queue!(self.inner, MoveTo(x, y))?;
            }
            last = Some(Position { x, y });
            let wanted = (cell.modifier, cell.fg, cell.bg);
            if wanted != style {
                self.inner
                    .write_all(sgr(cell.modifier, cell.fg, cell.bg).as_bytes())?;
                style = wanted;
            }
            self.inner.write_all(cell.symbol().as_bytes())?;
        }
        self.inner.write_all(b"\x1b[0m")
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}
