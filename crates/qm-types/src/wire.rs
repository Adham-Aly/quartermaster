//! The frames between the server and the terminals attached to it.
//!
//! One Unix socket, `<data dir>/server.sock`. A frame is `[u32 big-endian length][u8 kind][payload]`
//! where the length counts the kind byte and the payload, at most [`MAX_FRAME`]. The payload is
//! JSON for everything but [`Kind::Output`], which is raw terminal bytes. [`Hello`], [`Welcome`]
//! and [`Refuse`] are frozen: a client of any version can read the refusal of a server of any
//! other. Change [`PROTOCOL`] only when the set of frames changes.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol number the client and server must share.
pub const PROTOCOL: u32 = 1;

/// The most a frame (kind byte and payload) may hold: a big text paste fits.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// What a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// client → server, JSON [`Hello`].
    Hello = 1,
    /// server → client, JSON [`Welcome`].
    Welcome = 2,
    /// server → client, JSON [`Refuse`].
    Refuse = 3,
    /// client → server, JSON [`InputEvent`].
    Input = 4,
    /// client → server, JSON [`Size`].
    Resize = 5,
    /// client → server, empty: this window is leaving, the server stays.
    Detach = 6,
    /// server → client, raw bytes for the terminal.
    Output = 7,
    /// server → client, JSON [`Control`].
    Control = 8,
    /// CLI → server, JSON [`Request`].
    Request = 9,
    /// server → CLI, JSON [`Response`].
    Response = 10,
}

impl Kind {
    fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Hello,
            2 => Kind::Welcome,
            3 => Kind::Refuse,
            4 => Kind::Input,
            5 => Kind::Resize,
            6 => Kind::Detach,
            7 => Kind::Output,
            8 => Kind::Control,
            9 => Kind::Request,
            10 => Kind::Response,
            _ => return None,
        })
    }
}

/// One frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub payload: Vec<u8>,
}

/// Why a frame couldn't be read or written.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("the connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the other side sent a frame of {0} bytes, over the {MAX_FRAME} limit")]
    TooLarge(u32),
    #[error("the other side sent a frame of unknown kind {0}")]
    UnknownKind(u8),
    #[error("a frame didn't hold what its kind says: {0}")]
    Malformed(#[from] serde_json::Error),
}

impl Frame {
    /// A frame whose payload is `value` as JSON.
    ///
    /// # Panics
    /// Never in practice: the wire types serialize to JSON by construction.
    pub fn json<T: Serialize>(kind: Kind, value: &T) -> Frame {
        Frame {
            kind,
            payload: serde_json::to_vec(value).expect("wire types serialize"),
        }
    }

    /// A frame with no payload.
    pub fn empty(kind: Kind) -> Frame {
        Frame {
            kind,
            payload: Vec::new(),
        }
    }

    /// The payload read as JSON.
    pub fn parse<T: for<'de> Deserialize<'de>>(&self) -> Result<T, WireError> {
        Ok(serde_json::from_slice(&self.payload)?)
    }
}

/// Writes `frame` and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<(), WireError> {
    let len = u32::try_from(frame.payload.len() + 1).unwrap_or(u32::MAX);
    if len > MAX_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut buf = Vec::with_capacity(frame.payload.len() + 5);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(frame.kind as u8);
    buf.extend_from_slice(&frame.payload);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame; `Ok(None)` when the other side closed cleanly between frames.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>, WireError> {
    let mut head = [0u8; 4];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head);
    if len == 0 || len > MAX_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let kind = Kind::from_byte(body[0]).ok_or(WireError::UnknownKind(body[0]))?;
    body.remove(0);
    Ok(Some(Frame {
        kind,
        payload: body,
    }))
}

/// A terminal's size in cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// What a terminal's background looks like, from its answer to an OSC 11 query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    Light,
    Dark,
    #[default]
    Unknown,
}

/// The first frame of every attach. Frozen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub size: Size,
    pub ssh: bool,
    pub background: Background,
    pub keyboard_enhanced: bool,
    pub term: String,
}

/// The server accepts the attach. Frozen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Welcome {
    pub client_id: u64,
    pub version: String,
    pub protocol: u32,
}

/// The server turns the attach away. Frozen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Refuse {
    /// One sentence the user can act on.
    pub reason: String,
}

/// A key's modifier state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// What a terminal reports, as our own type so a terminal library upgrade never changes the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputEvent {
    /// A key: a character, or a name such as `enter`, `esc`, `up`, `f5`.
    Key {
        code: String,
        mods: Mods,
    },
    Mouse {
        kind: String,
        button: String,
        column: u16,
        row: u16,
        mods: Mods,
    },
    Paste {
        text: String,
    },
    Focus {
        gained: bool,
    },
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

/// Why a window is told to leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Only this window leaves; the server keeps running.
    Detached,
    ServerStopped,
    Restarting,
}

/// A server → window instruction that isn't terminal bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "control")]
pub enum Control {
    Suspend,
    Exit { reason: ExitReason },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let hello = Hello {
            protocol: PROTOCOL,
            version: "1.2.3".into(),
            ..Hello::default()
        };
        write_frame(&mut a, &Frame::json(Kind::Hello, &hello))
            .await
            .unwrap();
        write_frame(&mut a, &Frame::empty(Kind::Detach))
            .await
            .unwrap();
        drop(a);
        let f = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(f.kind, Kind::Hello);
        assert_eq!(f.parse::<Hello>().unwrap(), hello);
        assert_eq!(
            read_frame(&mut b).await.unwrap().unwrap().kind,
            Kind::Detach
        );
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversize_and_unknown_frames_are_errors() {
        let mut huge: &[u8] = &(MAX_FRAME + 1).to_be_bytes();
        assert!(matches!(
            read_frame(&mut huge).await,
            Err(WireError::TooLarge(_))
        ));
        let mut odd: &[u8] = &[0, 0, 0, 1, 99];
        assert!(matches!(
            read_frame(&mut odd).await,
            Err(WireError::UnknownKind(99))
        ));
    }
}
