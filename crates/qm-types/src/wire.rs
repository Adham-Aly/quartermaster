//! The wire between the UI host and a terminal client: frames, the handshake, our own input
//! event type, and the length-prefixed codec both ends use.
//!
//! A window is a thin client. It forwards what the user types and does ([`InputEvent`]) and prints
//! the bytes the host draws ([`Frame::Output`]); everything else, the whole UI, lives in the host.
//!
//! # Framing
//!
//! `[u32 big-endian length][u8 kind][payload]`, where the length counts the kind byte and the
//! payload, and is at most [`MAX_FRAME`]. [`Frame::Output`] carries raw bytes; every other payload
//! is JSON. [`Hello`], [`Welcome`] and [`Refuse`] are frozen: they are how two builds with
//! different frame sets find out they can't talk. [`InputEvent`] is ours, not crossterm's serde
//! form, so a crossterm upgrade never changes the wire.

use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol number. It changes only when the set of frames changes.
pub const PROTOCOL: u32 = 1;

/// The largest frame (kind byte and payload), in bytes: room for a big text paste.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// What the client's terminal background looks like, from an OSC 11 query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    Light,
    Dark,
    #[default]
    Unknown,
}

/// The colours a terminal reported for its text, background and 16 palette entries, as sRGB
/// bytes; `None` where it didn't answer. The host fits each window's palette to them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colors {
    #[serde(default)]
    pub fg: Option<[u8; 3]>,
    #[serde(default)]
    pub bg: Option<[u8; 3]>,
    #[serde(default)]
    pub ansi: Vec<Option<[u8; 3]>>,
    /// Whether the terminal takes 24-bit colour (`COLORTERM`), so the host may draw with the
    /// app's own palette rather than the terminal's 16.
    #[serde(default)]
    pub truecolor: bool,
    /// Whether the terminal shows images by the kitty graphics protocol, so the host may draw
    /// the logo.
    #[serde(default)]
    pub graphics: bool,
    /// One cell's size in pixels, `[width, height]`, when the terminal says: the logo is drawn
    /// at it.
    #[serde(default)]
    pub cell_pixels: Option<[u16; 2]>,
}

/// The client's first frame: who it is and what its terminal can do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub build: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub ssh: bool,
    #[serde(default)]
    pub background: Background,
    #[serde(default)]
    pub keyboard_enhanced: bool,
    #[serde(default)]
    pub term: String,
    #[serde(default)]
    pub colors: Colors,
    /// The terminal app the client runs in, by its bundle ID (`com.mitchellh.ghostty`), when the
    /// client knows it: notifications hold back while it is frontmost and a window in it has focus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_app: Option<String>,
}

impl Hello {
    /// A hello for a terminal of `cols` × `rows` at the current [`PROTOCOL`].
    #[must_use]
    pub fn new(version: &str, build: &str, cols: u16, rows: u16) -> Self {
        Self {
            protocol: PROTOCOL,
            version: version.to_owned(),
            build: build.to_owned(),
            cols,
            rows,
            ssh: false,
            background: Background::Unknown,
            keyboard_enhanced: false,
            term: String::new(),
            colors: Colors::default(),
            terminal_app: None,
        }
    }
}

/// The host's answer when it accepts a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Welcome {
    pub client_id: u64,
    pub version: String,
}

impl Welcome {
    #[must_use]
    pub fn new(client_id: u64, version: &str) -> Self {
        Self {
            client_id,
            version: version.to_owned(),
        }
    }
}

/// The host's answer when it turns a client away: one sentence the user can act on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Refuse {
    pub reason: String,
}

impl Refuse {
    #[must_use]
    pub fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_owned(),
        }
    }
}

/// Why a window is told to leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The UI finished: the user quit, or the core closed.
    Quit,
    /// Only this window leaves; the server keeps running.
    Detached,
    /// The server is stopping.
    ServerStopped,
    /// The server is restarting; the window should wait for it and attach again.
    Restarting,
}

/// A request from the command line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "request")]
pub enum Request {
    Status,
    /// Stop the server. Without `force`, a server with agents working answers
    /// [`Response::Working`] and keeps running.
    Stop {
        force: bool,
    },
    /// Stop and let the caller start it again.
    Restart {
        force: bool,
    },
}

/// The answer to a [`Request`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "response")]
pub enum Response {
    Status(ServerStatus),
    /// The server is stopping.
    Stopping,
    /// Nothing was stopped: this many agents are working and `force` wasn't set.
    Working {
        working: u32,
    },
}

/// What `server status` shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerStatus {
    pub pid: u32,
    pub version: String,
    pub protocol: u32,
    pub started_unix: i64,
    pub uptime_secs: u64,
    pub windows: u32,
    pub working: u32,
}

/// A host instruction to the client that isn't bytes to print.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// `Ctrl`+`Z`: leave full-screen mode, stop the process, and come back on `fg`.
    Suspend,
    /// Restore the terminal and return.
    Exit { reason: ExitReason },
    /// A notification was clicked and this is the window you used last: bring its own tab (or
    /// tmux window) forward, so the click lands on Quartermaster and not on another tab.
    Raise,
}

/// A key, independent of crossterm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    Char(char),
    F(u8),
    Enter,
    Backspace,
    Tab,
    BackTab,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
}

/// Whether a key went down, repeated, or came up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// Held modifier keys, as bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const SHIFT: u8 = 1;
    pub const CONTROL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;

    #[must_use]
    pub fn contains(self, bit: u8) -> bool {
        self.0 & bit == bit
    }
}

/// A mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// What the mouse did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    Moved,
    ScrollDown,
    ScrollUp,
    ScrollLeft,
    ScrollRight,
}

/// One thing the user did in a window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputEvent {
    Key {
        code: KeyCode,
        #[serde(default)]
        modifiers: Modifiers,
        #[serde(default)]
        kind: KeyKind,
    },
    Mouse {
        kind: MouseKind,
        column: u16,
        row: u16,
        #[serde(default)]
        modifiers: Modifiers,
    },
    Paste(String),
    FocusGained,
    FocusLost,
}

/// One frame on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Client → host: the first frame.
    Hello(Hello),
    /// Host → client: accepted.
    Welcome(Welcome),
    /// Host → client: turned away.
    Refuse(Refuse),
    /// Client → host: something the user did.
    Input(InputEvent),
    /// Client → host: the window's new size.
    Resize { cols: u16, rows: u16 },
    /// Client → host: the terminal's colours changed (it was resumed under a new theme).
    Colors(Colors),
    /// Client → host: this window is leaving.
    Detach,
    /// Host → client: bytes to write to the terminal as they are.
    Output(Vec<u8>),
    /// Host → client: an instruction.
    Control(Control),
    /// Command line → server.
    Request(Request),
    /// Server → command line.
    Response(Response),
}

/// The wire's failures.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("the connection failed: {0}")]
    Io(#[from] io::Error),
    #[error("a frame of {0} bytes is over the {MAX_FRAME}-byte limit")]
    TooLarge(u64),
    #[error("a frame of kind {0} is not one this build knows")]
    UnknownKind(u8),
    #[error("a frame could not be read: {0}")]
    Malformed(String),
}

const K_HELLO: u8 = 1;
const K_WELCOME: u8 = 2;
const K_REFUSE: u8 = 3;
const K_INPUT: u8 = 4;
const K_RESIZE: u8 = 5;
const K_DETACH: u8 = 6;
const K_OUTPUT: u8 = 7;
const K_CONTROL: u8 = 8;
const K_COLORS: u8 = 9;
const K_REQUEST: u8 = 10;
const K_RESPONSE: u8 = 11;

#[derive(Serialize, Deserialize)]
struct Size {
    cols: u16,
    rows: u16,
}

fn json<T: Serialize>(value: &T) -> Vec<u8> {
    // These types hold no maps with non-string keys, so serializing cannot fail.
    serde_json::to_vec(value).unwrap_or_default()
}

fn parse<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, WireError> {
    serde_json::from_slice(payload).map_err(|e| WireError::Malformed(e.to_string()))
}

impl Frame {
    /// The frame as `[kind][payload]`, without the length prefix.
    fn body(&self) -> (u8, Vec<u8>) {
        match self {
            Self::Hello(v) => (K_HELLO, json(v)),
            Self::Welcome(v) => (K_WELCOME, json(v)),
            Self::Refuse(v) => (K_REFUSE, json(v)),
            Self::Input(v) => (K_INPUT, json(v)),
            Self::Resize { cols, rows } => (
                K_RESIZE,
                json(&Size {
                    cols: *cols,
                    rows: *rows,
                }),
            ),
            Self::Detach => (K_DETACH, Vec::new()),
            Self::Output(bytes) => (K_OUTPUT, bytes.clone()),
            Self::Control(v) => (K_CONTROL, json(v)),
            Self::Colors(v) => (K_COLORS, json(v)),
            Self::Request(v) => (K_REQUEST, json(v)),
            Self::Response(v) => (K_RESPONSE, json(v)),
        }
    }

    /// The whole frame as bytes: length prefix, kind, payload.
    ///
    /// # Errors
    /// [`WireError::TooLarge`] if the frame is over [`MAX_FRAME`].
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let (kind, payload) = self.body();
        let len = payload.len() as u64 + 1;
        if len > u64::from(MAX_FRAME) {
            return Err(WireError::TooLarge(len));
        }
        let mut out = Vec::with_capacity(payload.len() + 5);
        out.extend_from_slice(&u32::try_from(len).unwrap_or(MAX_FRAME).to_be_bytes());
        out.push(kind);
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Reads a frame from its kind byte and payload.
    ///
    /// # Errors
    /// [`WireError::UnknownKind`] or [`WireError::Malformed`].
    pub fn decode(kind: u8, payload: &[u8]) -> Result<Self, WireError> {
        Ok(match kind {
            K_HELLO => Self::Hello(parse(payload)?),
            K_WELCOME => Self::Welcome(parse(payload)?),
            K_REFUSE => Self::Refuse(parse(payload)?),
            K_INPUT => Self::Input(parse(payload)?),
            K_RESIZE => {
                let Size { cols, rows } = parse(payload)?;
                Self::Resize { cols, rows }
            }
            K_DETACH => Self::Detach,
            K_OUTPUT => Self::Output(payload.to_vec()),
            K_CONTROL => Self::Control(parse(payload)?),
            K_COLORS => Self::Colors(parse(payload)?),
            K_REQUEST => Self::Request(parse(payload)?),
            K_RESPONSE => Self::Response(parse(payload)?),
            other => return Err(WireError::UnknownKind(other)),
        })
    }
}

/// Writes one frame and flushes.
///
/// # Errors
/// A failed write, or a frame over [`MAX_FRAME`].
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<(), WireError> {
    w.write_all(&frame.encode()?).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame. `Ok(None)` is a clean close between frames.
///
/// # Errors
/// A failed read, a close in the middle of a frame, an oversized, unknown or malformed frame.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>, WireError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len);
    if len == 0 {
        return Err(WireError::Malformed("an empty frame".to_owned()));
    }
    if len > MAX_FRAME {
        return Err(WireError::TooLarge(u64::from(len)));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Frame::decode(body[0], &body[1..]).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> Vec<Frame> {
        vec![
            Frame::Hello(Hello::new("0.1.0", "abc", 150, 45)),
            Frame::Welcome(Welcome::new(3, "0.1.0")),
            Frame::Refuse(Refuse::new("no")),
            Frame::Input(InputEvent::Key {
                code: KeyCode::Char('é'),
                modifiers: Modifiers(Modifiers::CONTROL | Modifiers::SHIFT),
                kind: KeyKind::Press,
            }),
            Frame::Input(InputEvent::Mouse {
                kind: MouseKind::Drag(MouseButton::Left),
                column: 9,
                row: 4,
                modifiers: Modifiers::default(),
            }),
            Frame::Input(InputEvent::Paste("a\nb".to_owned())),
            Frame::Input(InputEvent::FocusLost),
            Frame::Resize { cols: 60, rows: 16 },
            Frame::Colors(Colors {
                fg: Some([1, 2, 3]),
                bg: None,
                ansi: vec![Some([9, 9, 9]), None],
                truecolor: true,
                graphics: true,
                cell_pixels: Some([16, 34]),
            }),
            Frame::Detach,
            Frame::Request(Request::Stop { force: true }),
            Frame::Response(Response::Working { working: 2 }),
            Frame::Output(vec![0x1b, b'[', 0, 255]),
            Frame::Control(Control::Suspend),
            Frame::Control(Control::Exit {
                reason: ExitReason::Quit,
            }),
        ]
    }

    #[tokio::test]
    async fn every_frame_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        for f in frames() {
            write_frame(&mut a, &f).await.unwrap();
            assert_eq!(read_frame(&mut b).await.unwrap(), Some(f));
        }
        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bad_frames_are_errors() {
        let mut oversized: &[u8] = &(MAX_FRAME + 1).to_be_bytes();
        assert!(matches!(
            read_frame(&mut oversized).await,
            Err(WireError::TooLarge(_))
        ));
        let mut unknown: &[u8] = &[0, 0, 0, 1, 99];
        assert!(matches!(
            read_frame(&mut unknown).await,
            Err(WireError::UnknownKind(99))
        ));
        let mut bad: &[u8] = &[0, 0, 0, 3, K_HELLO, b'{', b'x'];
        assert!(matches!(
            read_frame(&mut bad).await,
            Err(WireError::Malformed(_))
        ));
        let mut cut: &[u8] = &[0, 0, 0, 9, K_OUTPUT, 1];
        assert!(matches!(read_frame(&mut cut).await, Err(WireError::Io(_))));
    }

    #[test]
    fn hello_ignores_unknown_and_defaults_missing_fields() {
        let h: Hello = serde_json::from_str(
            r#"{"protocol":1,"version":"v","build":"b","cols":1,"rows":2,"future":true}"#,
        )
        .unwrap();
        assert_eq!(h.background, Background::Unknown);
        assert!(!h.ssh);
    }
}
