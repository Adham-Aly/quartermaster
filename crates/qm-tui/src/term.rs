//! The state of one terminal window: its size, what it last drew, where its pointer is, and the
//! small things that only make sense for the window they happened in. `App` and `View` keep the
//! state every window shares.

use crate::app::Quit;
use crate::hit::{Divider, FrameMap, Hit};
use crate::theme::Palette;
use crate::view::{Flash, Selection};

/// Whether a terminal's background is light or dark, when known.
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
    /// The window asked to leave (the server keeps running).
    pub(crate) detach: bool,
    /// `Ctrl`+`Z` was pressed: the loop suspends to the shell before the next frame.
    pub(crate) suspend: bool,
    /// The window is over SSH, so copies go by OSC 52 and the Mac's clipboard is out of reach.
    pub(crate) ssh: bool,
    /// The window's background, for choosing colours.
    pub(crate) background: Background,
    /// The colours this window is drawn with, fitted to its terminal (`theme::scoped` per frame).
    pub(crate) palette: Palette,
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
            detach: false,
            suspend: false,
            ssh: crate::input::clipboard::is_ssh(tty.as_deref(), connection.as_deref()),
            background: Background::Unknown,
            palette: Palette::DEFAULT,
        }
    }
}
