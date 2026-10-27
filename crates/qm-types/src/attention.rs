//! What waits on you, as the UI host tells the notifier (`qm-notify`), and the clicks that come
//! back.
//!
//! The host rebuilds a [`Waiting`] whenever the Actions inbox, the windows' focus or the setting
//! changes; the notifier diffs it to post a banner for each new request and withdraw the banners
//! of answered ones. A click on a banner or a menu entry comes back as the request's [`ActionKey`]
//! and the host opens its card. The notifier says how it is doing ([`Health`]), which Settings
//! shows.

use crate::api::{ActionKey, ActionKind};
use jiff::Timestamp;
use tokio::sync::{mpsc, watch};

/// Everything the notifier needs: what is waiting, where you are looking, and whether it is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Waiting {
    /// The notifications setting.
    pub enabled: bool,
    /// The Actions inbox, in its order.
    pub items: Vec<WaitingItem>,
    pub looking: Looking,
    /// The terminal app (its bundle ID) a click brings forward: the window focused last.
    pub terminal: Option<String>,
}

impl Waiting {
    pub fn new(enabled: bool, items: Vec<WaitingItem>, looking: Looking) -> Self {
        Self {
            enabled,
            items,
            looking,
            terminal: None,
        }
    }

    #[must_use]
    pub fn with_terminal(mut self, terminal: Option<String>) -> Self {
        self.terminal = terminal;
        self
    }
}

/// One request in the inbox, in the words a banner shows.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaitingItem {
    pub key: ActionKey,
    pub kind: ActionKind,
    /// The inbox line: "Merge PR #41 · feat(billing): proration preview".
    pub title: String,
    /// Who asked: `C-0042`, `S-02`, `Quartermaster`; `None` for the app's own prompts.
    pub from: Option<String>,
    /// When it started waiting on you.
    pub at: Timestamp,
}

impl WaitingItem {
    pub fn new(key: ActionKey, kind: ActionKind, title: impl Into<String>, at: Timestamp) -> Self {
        Self {
            key,
            kind,
            title: title.into(),
            from: None,
            at,
        }
    }

    #[must_use]
    pub fn with_from(mut self, from: Option<String>) -> Self {
        self.from = from;
        self
    }
}

/// Whether you are looking at Quartermaster, which holds back banners.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Looking {
    /// No window has focus (or none is attached): every new request posts a banner.
    #[default]
    Away,
    /// Windows have focus in these terminal apps (bundle IDs): a banner is held back while one of
    /// them is the frontmost app.
    In(Vec<String>),
    /// A window has focus in a terminal the host can't name: banners are held back.
    Here,
}

/// How the notifier is doing, as Settings tells you.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Health {
    /// Notifications are off, or the notifier hasn't started.
    #[default]
    Off,
    /// The helper app is starting (or starting again).
    Starting,
    /// Banners show.
    Ready,
    /// Banners show, but macOS hides their text (Show previews is Never for the app).
    TextHidden,
    /// macOS doesn't allow the app's banners; the menu bar mark still shows.
    Denied,
    /// The helper app can't run; the log says why, and it is tried again later.
    Failed,
}

/// The host's end: it publishes what is waiting and hears which request was clicked and how the
/// notifier is doing.
#[derive(Debug)]
pub struct HostEnd {
    pub waiting: watch::Sender<Waiting>,
    pub opened: mpsc::UnboundedReceiver<ActionKey>,
    pub health: watch::Receiver<Health>,
}

/// The notifier's end.
#[derive(Debug)]
pub struct NotifierEnd {
    pub waiting: watch::Receiver<Waiting>,
    pub opened: mpsc::UnboundedSender<ActionKey>,
    pub health: watch::Sender<Health>,
}

/// A connected pair of ends.
pub fn link() -> (HostEnd, NotifierEnd) {
    let (waiting_tx, waiting_rx) = watch::channel(Waiting::default());
    let (opened_tx, opened_rx) = mpsc::unbounded_channel();
    let (health_tx, health_rx) = watch::channel(Health::Off);
    (
        HostEnd {
            waiting: waiting_tx,
            opened: opened_rx,
            health: health_rx,
        },
        NotifierEnd {
            waiting: waiting_rx,
            opened: opened_tx,
            health: health_tx,
        },
    )
}
