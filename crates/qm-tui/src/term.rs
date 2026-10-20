//! The state of one terminal window: its size, what it last drew, where its pointer is, and the
//! small things that only make sense for the window they happened in. `App` and `View` keep the
//! state every window shares.

use crate::app::Quit;
use crate::hit::{Divider, FrameMap, Hit};
use crate::view::{Flash, Selection};

/// Whether a terminal's background is light or dark, when known.
#[expect(dead_code, reason = "the light-mode work will set and read these")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Background {
    #[default]
    Unknown,
    Light,
    Dark,
}

/// Per-window state, held in `App::term`.
#[derive(Debug)]
pub(crate) struct Term {
    /// The window's size at the last frame.
    pub(crate) size: (u16, u16),
    /// What the last frame drew.
    pub(crate) drawn: FrameMap,
    /// What the pointer is over, from the last frame's hit map.
    pub(crate) hover: Option<Hit>,
    /// The divider being dragged, if any.
    pub(crate) drag: Option<Divider>,
    /// The left button's recent history: double-clicks and drag-selections.
    pub(crate) pointer: crate::input::mouse::Pointer,
    /// An OSC 52 copy to write to the terminal right after the next frame.
    pub(crate) osc52: Option<String>,
    /// The core rang the bell: how many BELs to write to the terminal (at most one).
    pub(crate) bells: u8,
    /// Where quitting stands, including the first `Ctrl`+`C` waiting for its second.
    pub(crate) quit: Quit,
    /// The app's own text selection, in this window's screen cells, while dragging and until the
    /// next click.
    pub(crate) selection: Option<Selection>,
    /// The status bar's short notice ("Copied").
    pub(crate) flash: Option<Flash>,
    /// `Ctrl`+`Z` was pressed: the loop suspends to the shell before the next frame.
    pub(crate) suspend: bool,
    /// The window is over SSH, so copies go by OSC 52 and the Mac's clipboard is out of reach.
    pub(crate) ssh: bool,
    /// The window's background, for choosing colours.
    #[expect(
        dead_code,
        reason = "recorded for the light-mode work; nothing reads it yet"
    )]
    pub(crate) background: Background,
}

impl Term {
    /// A window with nothing drawn yet; `ssh` is read from this process's environment.
    pub(crate) fn new() -> Self {
        let tty = std::env::var("SSH_TTY").ok();
        let connection = std::env::var("SSH_CONNECTION").ok();
        Self {
            size: (0, 0),
            drawn: FrameMap::default(),
            hover: None,
            drag: None,
            pointer: crate::input::mouse::Pointer::default(),
            osc52: None,
            bells: 0,
            quit: Quit::Idle,
            selection: None,
            flash: None,
            suspend: false,
            ssh: crate::input::clipboard::is_ssh(tty.as_deref(), connection.as_deref()),
            background: Background::Unknown,
        }
    }
}
