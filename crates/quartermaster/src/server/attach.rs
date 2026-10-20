//! The attach client: puts this terminal in full-screen mode and forwards its keys to the server.
//!
//! It draws nothing itself. The server renders for this window's size and sends the bytes; the
//! client writes them. Closing the window, or the server telling this window to leave, detaches
//! it; the server and its agents keep running.

use super::spawn;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use qm_types::data_dir::DataDir;
use qm_types::wire::{
    self, Background, Control, ExitReason, Frame, Hello, InputEvent, Kind, Mods, PROTOCOL, Refuse,
    Size,
};
use std::io::Write;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// How one attach ended.
enum Ended {
    Left(ExitReason),
    /// The server went away without saying why.
    Lost,
}

/// Restores the terminal when dropped, on every way out.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<TerminalGuard> {
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        crossterm::execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Attaches until this window is told to leave. Returns what to print after the terminal is back.
pub async fn run(dir: &DataDir) -> Result<String, String> {
    let mut last_lost: Option<Instant> = None;
    // Terminal events are read on one thread for the life of the process and forwarded as frames.
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if let Some(frame) = input_of(event)
                && tx.send(frame).is_err()
            {
                break;
            }
        }
    });
    let guard = TerminalGuard::enter().map_err(|e| format!("can't take over the terminal: {e}"))?;
    let message = loop {
        let stream = spawn::connect_or_start(dir).await?;
        match session(stream, &mut rx).await? {
            Ended::Left(ExitReason::Detached) => {
                break "Detached. Quartermaster keeps running; run `quartermaster` to come back."
                    .to_owned();
            }
            Ended::Left(ExitReason::ServerStopped) => break "Quartermaster stopped.".to_owned(),
            Ended::Left(ExitReason::Restarting) => {
                let mut out = std::io::stdout();
                let _ = write!(out, "\x1b[2J\x1b[HRestarting…");
                let _ = out.flush();
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Ended::Lost => {
                // Start it once more; a second loss within 30 s shows the error instead.
                if last_lost.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
                    break format!(
                        "The Quartermaster server stopped unexpectedly twice; the log in {} says why.",
                        dir.logs().display()
                    );
                }
                last_lost = Some(Instant::now());
            }
        }
    };
    drop(guard);
    Ok(message)
}

fn key_name(key: &KeyEvent) -> Option<String> {
    Some(match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    })
}

fn input_of(event: Event) -> Option<Frame> {
    let input = match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => InputEvent::Key {
            code: key_name(&key)?,
            mods: Mods {
                shift: key.modifiers.contains(KeyModifiers::SHIFT),
                ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
                alt: key.modifiers.contains(KeyModifiers::ALT),
            },
        },
        Event::Paste(text) => InputEvent::Paste { text },
        Event::FocusGained => InputEvent::Focus { gained: true },
        Event::FocusLost => InputEvent::Focus { gained: false },
        Event::Resize(cols, rows) => {
            return Some(Frame::json(Kind::Resize, &Size { cols, rows }));
        }
        _ => return None,
    };
    Some(Frame::json(Kind::Input, &input))
}

async fn session(
    stream: UnixStream,
    rx: &mut mpsc::UnboundedReceiver<Frame>,
) -> Result<Ended, String> {
    let (mut rd, mut wr) = stream.into_split();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let hello = Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        size: Size { cols, rows },
        ssh: ["SSH_TTY", "SSH_CONNECTION"]
            .iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty())),
        background: Background::Unknown,
        keyboard_enhanced: false,
        term: std::env::var("TERM").unwrap_or_default(),
    };
    let io = |e: wire::WireError| e.to_string();
    wire::write_frame(&mut wr, &Frame::json(Kind::Hello, &hello))
        .await
        .map_err(io)?;
    match wire::read_frame(&mut rd).await.map_err(io)? {
        Some(f) if f.kind == Kind::Welcome => {}
        Some(f) if f.kind == Kind::Refuse => {
            return Err(f.parse::<Refuse>().map_err(io)?.reason);
        }
        _ => return Ok(Ended::Lost),
    }

    let mut stdout = std::io::stdout();
    loop {
        tokio::select! {
            frame = rx.recv() => {
                let Some(frame) = frame else { return Ok(Ended::Lost) };
                if wire::write_frame(&mut wr, &frame).await.is_err() {
                    return Ok(Ended::Lost);
                }
            }
            frame = wire::read_frame(&mut rd) => match frame {
                Ok(Some(f)) => match f.kind {
                    Kind::Output => {
                        let _ = stdout.write_all(&f.payload);
                        let _ = stdout.flush();
                    }
                    Kind::Control => {
                        if let Ok(Control::Exit { reason }) = f.parse() {
                            return Ok(Ended::Left(reason));
                        }
                    }
                    _ => {}
                },
                Ok(None) | Err(_) => return Ok(Ended::Lost),
            },
        }
    }
}
