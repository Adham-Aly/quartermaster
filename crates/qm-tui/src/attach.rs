//! The thin client: one terminal window attached to the host.
//!
//! It puts the terminal in full-screen mode (the same [`TerminalGuard`] and panic hook as ever),
//! says hello with the window's size, then forwards what the user does as wire [`InputEvent`]s
//! and writes the bytes the host draws. It holds no UI state of its own.

use crate::convert;
use crate::terminal::TerminalGuard;
use crossterm::event::{self, Event, MouseButton, MouseEventKind};
use qm_types::wire::{
    self, Background, Control, ExitReason, Frame, Hello, read_frame, write_frame,
};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// How often the event thread looks up to see whether the client has gone.
const WATCH: Duration = Duration::from_millis(100);

/// A one-row bar this client draws along the bottom of its window, under what the host draws,
/// with one button. The host knows nothing of it: the window it draws for is one row shorter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub button: String,
}

/// How an attached window ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// The host said to leave, and why.
    Told(ExitReason),
    /// The stream closed without a word: the host is gone.
    Lost,
    /// The user pressed the [`Notice`]'s button.
    Pressed,
}

impl Notice {
    /// The bar drawn on the terminal's last row, as bytes; the cursor is left where it was.
    fn bytes(&self, cols: u16, row: u16) -> Vec<u8> {
        let button = format!(" {} ", self.button);
        let text = self.shown(cols);
        // Save the cursor, go to the row, clear it, then the text and the reversed button.
        format!(
            "\x1b7\x1b[{row};1H\x1b[2K \x1b[33m{text}\x1b[0m  \x1b[7m{button}\x1b[0m\x1b8",
            row = row + 1
        )
        .into_bytes()
    }

    /// The text as drawn in `cols` columns.
    fn shown(&self, cols: u16) -> String {
        let button = self.button.chars().count() + 2;
        let room = usize::from(cols).saturating_sub(button + 2);
        self.text.chars().take(room).collect()
    }

    /// Whether a click at `column` is on the button, in a terminal `cols` wide.
    fn on_button(&self, column: u16, cols: u16) -> bool {
        let start = self.shown(cols).chars().count() + 3;
        let end = start + self.button.chars().count() + 2;
        (start..end).contains(&usize::from(column))
    }
}

/// The size the host draws for: the terminal's, less the notice's row.
fn host_size(cols: u16, rows: u16, notice: Option<&Notice>) -> (u16, u16) {
    (
        cols,
        if notice.is_some() {
            rows.saturating_sub(1).max(1)
        } else {
            rows
        },
    )
}

fn failed(error: &wire::WireError) -> io::Error {
    io::Error::other(error.to_string())
}

/// Reads the terminal's events on a thread of its own (crossterm blocks) and forwards them.
fn read_events() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while !tx.is_closed() {
            match event::poll(WATCH) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                },
                Ok(false) => {}
                Err(_) => return,
            }
        }
    });
    rx
}

fn hello(cols: u16, rows: u16, colors: wire::Colors) -> Hello {
    let mut hello = Hello::new(env!("CARGO_PKG_VERSION"), "", cols, rows);
    hello.ssh =
        std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some();
    hello.keyboard_enhanced = TerminalGuard::keyboard_enhanced();
    hello.background = Background::Unknown;
    hello.colors = colors;
    hello.term = std::env::var("TERM").unwrap_or_default();
    hello
}

/// Attaches this terminal to the host on `stream` and returns when the host says to leave (with its
/// reason), the stream closes (`None`), or the terminal fails; the terminal is restored either way.
///
/// # Errors
/// The terminal can't enter full-screen mode, the host refuses this window (the sentence it gives
/// is the error), or the stream fails.
pub async fn attach<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    notice: Option<Notice>,
) -> io::Result<Left> {
    let mut guard = TerminalGuard::enter()?;
    let (mut read, mut write) = tokio::io::split(stream);
    let (cols, rows) = crossterm::terminal::size()?;
    let (host_cols, host_rows) = host_size(cols, rows, notice.as_ref());
    write_frame(
        &mut write,
        &Frame::Hello(hello(host_cols, host_rows, guard.colors())),
    )
    .await
    .map_err(|e| failed(&e))?;
    match read_frame(&mut read).await.map_err(|e| failed(&e))? {
        Some(Frame::Welcome(_)) => {}
        Some(Frame::Refuse(refuse)) => {
            drop(guard);
            return Err(io::Error::other(refuse.reason));
        }
        _ => {
            drop(guard);
            return Err(io::Error::other("Quartermaster didn't answer."));
        }
    }

    // The host's frames arrive on a task of their own, so a slow write to the terminal never
    // blocks reading the stream, and the terminal's events can be awaited next to them.
    let (frames_tx, mut frames) = mpsc::unbounded_channel();
    let pump = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut read).await {
            if frames_tx.send(frame).is_err() {
                return;
            }
        }
    });
    let mut events = read_events();

    let outcome = async {
        loop {
            tokio::select! {
                frame = frames.recv() => match frame {
                    Some(Frame::Output(mut bytes)) => {
                        if let Some(notice) = &notice {
                            let (cols, rows) = crossterm::terminal::size()?;
                            bytes.extend(notice.bytes(cols, rows.saturating_sub(1)));
                        }
                        guard.write_raw(&bytes)?;
                    }
                    Some(Frame::Control(Control::Suspend)) => {
                        let colors = TerminalGuard::suspend()?;
                        write_frame(&mut write, &Frame::Colors(colors))
                            .await
                            .map_err(|e| failed(&e))?;
                        // Entering again cleared nothing the host knows about: ask for a full
                        // repaint.
                        let (cols, rows) = crossterm::terminal::size()?;
                        let (cols, rows) = host_size(cols, rows, notice.as_ref());
                        write_frame(&mut write, &Frame::Resize { cols, rows })
                            .await
                            .map_err(|e| failed(&e))?;
                    }
                    Some(Frame::Control(Control::Exit { reason })) => return Ok(Left::Told(reason)),
                    None => return Ok(Left::Lost),
                    Some(_) => {}
                },
                event = events.recv() => {
                    let Some(event) = event else { return Ok(Left::Told(ExitReason::Detached)) };
                    let frame = match event {
                        Event::Resize(cols, rows) => {
                            let (cols, rows) = host_size(cols, rows, notice.as_ref());
                            Frame::Resize { cols, rows }
                        }
                        // The notice's row is this client's: its button works, nothing there
                        // reaches the host.
                        Event::Mouse(mouse)
                            if notice.is_some()
                                && mouse.row == crossterm::terminal::size()?.1.saturating_sub(1) =>
                        {
                            if let Some(notice) = &notice
                                && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                                && notice.on_button(mouse.column, crossterm::terminal::size()?.0)
                            {
                                return Ok(Left::Pressed);
                            }
                            continue;
                        }
                        other => match convert::to_wire(&other) {
                            Some(input) => Frame::Input(input),
                            None => continue,
                        },
                    };
                    write_frame(&mut write, &frame).await.map_err(|e| failed(&e))?;
                }
            }
        }
    }
    .await;
    pump.abort();
    drop(guard);
    outcome
}
