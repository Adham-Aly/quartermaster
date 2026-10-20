//! The embedded terminal pane: a pseudo-terminal running one install or login, its screen kept by
//! `vt100` and drawn inside ratatui in the terminal's own 16 colours.
//!
//! Nothing here runs by itself: a [`Pane`] starts only when you click a setup button (or press
//! its key), runs exactly the command the core named, never `sudo`, and stops when you cancel or
//! when the app quits (dropping a pane kills what it runs, so nothing outlives the app). Your keys
//! go to the program while the pane is focused, so interactive logins (`gh auth login`, `claude
//! auth login`) work as in any terminal.

use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Lines of scrollback the pane keeps (only the screen is drawn).
const SCROLLBACK: usize = 0;

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Exit {
    pub(crate) code: u32,
    pub(crate) success: bool,
    /// You cancelled it.
    pub(crate) cancelled: bool,
}

/// One command running in a pseudo-terminal.
pub(crate) struct Pane {
    screen: Arc<Mutex<vt100::Parser>>,
    /// Output arrived since the last frame.
    dirty: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Rows and columns.
    size: (u16, u16),
    exit: Option<Exit>,
    cancelled: bool,
}

impl std::fmt::Debug for Pane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pane")
            .field("size", &self.size)
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

fn non_zero(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap_or(NonZeroU16::MIN)
}

impl Pane {
    /// Starts `argv` in a pseudo-terminal of `rows` × `cols`, in your home folder, with the app's
    /// environment. Refuses `sudo`.
    pub(crate) fn spawn(argv: &[String], rows: u16, cols: u16) -> Result<Self, String> {
        let Some(program) = argv.first() else {
            return Err("there's nothing to run".to_owned());
        };
        let name = Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == "sudo" || argv.iter().any(|a| a == "sudo") {
            return Err("the setup check never runs anything with sudo".to_owned());
        }
        let (rows, cols) = (rows.max(2), cols.max(10));
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("couldn't open a terminal pane: {e}"))?;
        let mut command = CommandBuilder::new(program);
        command.args(&argv[1..]);
        command.env("TERM", "xterm-256color");
        // Homebrew's output plain, without its colours and bold headings.
        command.env("HOMEBREW_NO_COLOR", "1");
        // The same login an agent would see: no stray API key from your shell.
        for key in qm_types::env::NEVER_PASSED_TO_AGENTS {
            command.env_remove(key);
        }
        if let Some(home) = std::env::var_os("HOME") {
            command.cwd(home);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("couldn't start {program}: {e}"))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("couldn't read the terminal pane: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("couldn't write to the terminal pane: {e}"))?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            non_zero(rows),
            non_zero(cols),
            SCROLLBACK,
        )));
        let dirty = Arc::new(AtomicBool::new(true));
        {
            let screen = Arc::clone(&screen);
            let dirty = Arc::clone(&dirty);
            std::thread::Builder::new()
                .name("qm-setup-pane".into())
                .spawn(move || {
                    let mut buf = [0_u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                screen
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .process(&buf[..n]);
                                dirty.store(true, Ordering::Release);
                            }
                        }
                    }
                    dirty.store(true, Ordering::Release);
                })
                .map_err(|e| format!("couldn't start reading the terminal pane: {e}"))?;
        }
        Ok(Self {
            screen,
            dirty,
            master: pair.master,
            writer,
            child,
            size: (rows, cols),
            exit: None,
            cancelled: false,
        })
    }

    /// Whether output arrived since the last call.
    pub(crate) fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// The exit, once the program has ended. Returns `Some` from then on.
    pub(crate) fn poll(&mut self) -> Option<Exit> {
        if self.exit.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit = Some(Exit {
                code: status.exit_code(),
                success: status.success() && !self.cancelled,
                cancelled: self.cancelled,
            });
        }
        self.exit
    }

    /// How it ended, once it has (as last polled).
    pub(crate) fn exit(&self) -> Option<Exit> {
        self.exit
    }

    pub(crate) fn running(&self) -> bool {
        self.exit.is_none()
    }

    /// Stops the program (you pressed Cancel or `Esc`).
    pub(crate) fn cancel(&mut self) {
        if self.exit.is_none() {
            self.cancelled = true;
            let _ = self.child.kill();
        }
    }

    /// Sends your typing to the program.
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        if self.exit.is_none() && !bytes.is_empty() {
            let _ = self.writer.write_all(bytes);
            let _ = self.writer.flush();
        }
    }

    /// Fits the pseudo-terminal to `rows` × `cols`.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(10));
        if (rows, cols) == self.size {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_size(non_zero(rows), non_zero(cols));
        self.dirty.store(true, Ordering::Release);
    }

    pub(crate) fn size(&self) -> (u16, u16) {
        self.size
    }

    /// How many rows of the screen hold something (the last non-blank row, plus one).
    pub(crate) fn used_rows(&self) -> u16 {
        let parser = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        (0..rows.get())
            .rev()
            .find(|&row| {
                (0..cols.get()).any(|col| {
                    screen
                        .cell(row, col)
                        .is_some_and(|c| !c.contents().trim().is_empty())
                })
            })
            .map_or(0, |row| row + 1)
    }

    /// Draws screen rows `from..` into `area`; returns where the cursor is when `cursor` asks
    /// for it and the program shows one.
    pub(crate) fn draw(
        &self,
        buf: &mut Buffer,
        area: Rect,
        from: u16,
        cursor: bool,
    ) -> Option<Position> {
        let parser = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        for y in 0..area.height {
            let row = from + y;
            if row >= rows.get() {
                break;
            }
            let mut x = 0;
            while x < area.width.min(cols.get()) {
                let Some(cell) = screen.cell(row, x) else {
                    break;
                };
                if cell.is_wide_continuation() {
                    x += 1;
                    continue;
                }
                let text = cell.contents();
                let text = if text.is_empty() { " " } else { text };
                buf.set_string(area.x + x, area.y + y, text, style_of(cell));
                x += if cell.is_wide() { 2 } else { 1 };
            }
        }
        if !cursor || screen.hide_cursor() {
            return None;
        }
        let (row, col) = screen.cursor_position();
        (row >= from && row - from < area.height && col < area.width)
            .then(|| Position::new(area.x + col, area.y + row - from))
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.exit.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A cell's style in the palette roles: output is dim, and what a program
/// emphasises (bold, like `gh`'s one-time code) takes the accent, bold. A program's own colours
/// aren't used, so every pane reads the same in light and dark themes; reverse video and underlines
/// stay, so a menu's selection still shows.
fn style_of(cell: &vt100::Cell) -> Style {
    let mut style = if cell.bold() {
        theme::accent().add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    };
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// The bytes a key sends to a program in a terminal.
pub(crate) fn key_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            vec![(c.to_ascii_lowercase() as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => Vec::new(),
    };
    if alt && !bytes.is_empty() {
        bytes.insert(0, 0x1b);
    }
    bytes
}
