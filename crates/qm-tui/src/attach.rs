//! The thin client: one terminal window attached to the host.
//!
//! It puts the terminal in full-screen mode (the same [`TerminalGuard`] and panic hook as ever),
//! says hello with the window's size, then forwards what the user does as wire [`InputEvent`]s
//! and writes the bytes the host draws. It holds no UI state of its own.

use crate::convert;
use crate::terminal::TerminalGuard;
use crossterm::event::{self, Event};
use qm_types::wire::{
    self, Background, Control, ExitReason, Frame, Hello, read_frame, write_frame,
};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// How often the event thread looks up to see whether the client has gone.
const WATCH: Duration = Duration::from_millis(100);

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
) -> io::Result<Option<ExitReason>> {
    let mut guard = TerminalGuard::enter()?;
    let (mut read, mut write) = tokio::io::split(stream);
    let (cols, rows) = crossterm::terminal::size()?;
    write_frame(&mut write, &Frame::Hello(hello(cols, rows, guard.colors())))
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
                    Some(Frame::Output(bytes)) => guard.write_raw(&bytes)?,
                    Some(Frame::Control(Control::Suspend)) => {
                        let colors = TerminalGuard::suspend()?;
                        write_frame(&mut write, &Frame::Colors(colors))
                            .await
                            .map_err(|e| failed(&e))?;
                        // Entering again cleared nothing the host knows about: ask for a full
                        // repaint.
                        let (cols, rows) = crossterm::terminal::size()?;
                        write_frame(&mut write, &Frame::Resize { cols, rows })
                            .await
                            .map_err(|e| failed(&e))?;
                    }
                    Some(Frame::Control(Control::Exit { reason })) => return Ok(Some(reason)),
                    None => return Ok(None),
                    Some(_) => {}
                },
                event = events.recv() => {
                    let Some(event) = event else { return Ok(Some(ExitReason::Detached)) };
                    let frame = match event {
                        Event::Resize(cols, rows) => Frame::Resize { cols, rows },
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
