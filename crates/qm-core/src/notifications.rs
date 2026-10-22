//! Getting your attention: the terminal bell and a macOS desktop notification, only for questions,
//! decisions and approvals, and never when notifications are switched off (`[notifications] enabled
//! = false` in `config.toml`).
//!
//! [`NotificationsPart`] is an [`ApiPart`] filled **after every other part**, so it sees each
//! snapshot exactly as the Bridge will draw it. Every line that waits on you (a question from
//! `ask_user`, an unanswered approval card, an unanswered decision card) is noticed once, by its
//! message ID, the first time a snapshot carries it, wherever it came from: the seat's
//! `ask_user`, a held merge, push or GitHub write, a decision card. Ordinary chat (replies, wake
//! turns, notices) never notifies.
//!
//! For each new one, when notifications are on, the part
//! - raises [`Snapshot::bell`] by one; the UI writes BEL to the terminal when it rises; and
//! - hands a [`Notification`] to its [`NotificationSink`], which on the Mac is
//!   [`DesktopNotifier`] (`osascript`'s `display notification`). Tests give it a fake sink.
//!
//! The first snapshot after launch is the baseline: what was already waiting when the app
//! started is shown, not announced again.

use crate::api::{ApiPart, Handled};
use qm_types::BoxFuture;
use qm_types::api::{CardKind, ChatLine, Command, Snapshot};
use qm_types::config::Config;
use qm_types::domain::MessageKind;
use qm_types::ids::MessageId;
use std::collections::HashSet;
use std::fmt;
use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::watch;

/// The notification's title.
pub const TITLE: &str = "Quartermaster";
/// The longest body a notification shows, in characters.
pub const MAX_BODY_CHARS: usize = 200;

/// What waits on you.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttentionKind {
    /// A question from `ask_user`.
    Question,
    /// A decision card.
    Decision,
    /// An approval card (a held merge, push, GitHub write or new project).
    Approval,
}

impl AttentionKind {
    /// The notification's subtitle.
    pub fn label(self) -> &'static str {
        match self {
            Self::Question => "Question",
            Self::Decision => "Decision",
            Self::Approval => "Approval needed",
        }
    }
}

/// One desktop notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub kind: AttentionKind,
    /// The chat line it is about.
    pub message: MessageId,
    /// [`TITLE`].
    pub title: String,
    /// "Question", "Decision" or "Approval needed".
    pub subtitle: String,
    /// The question, or the card's heading and title, on one line.
    pub body: String,
}

/// Where desktop notifications go.
pub trait NotificationSink: Send + Sync + fmt::Debug {
    /// Shows `notification`; never blocks and never fails loudly.
    fn notify(&self, notification: &Notification);
}

/// The Mac's desktop notification, through `osascript`. The text is passed as arguments to the
/// script, never spliced into it; the process runs on its own thread, and a failure is logged.
#[derive(Clone, Copy, Debug, Default)]
pub struct DesktopNotifier;

const SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 1 of argv) with title (item 2 of argv) subtitle (item 3 of argv)",
    "end run",
];

impl NotificationSink for DesktopNotifier {
    fn notify(&self, notification: &Notification) {
        let mut cmd = Process::new("/usr/bin/osascript");
        for line in SCRIPT {
            cmd.arg("-e").arg(line);
        }
        cmd.arg(&notification.body)
            .arg(&notification.title)
            .arg(&notification.subtitle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let spawned = std::thread::Builder::new()
            .name("qm-notify".to_owned())
            .spawn(move || match cmd.status() {
                Ok(status) if status.success() => {}
                Ok(status) => tracing::debug!(%status, "osascript couldn't show a notification"),
                Err(e) => tracing::debug!(error = %e, "osascript couldn't be run"),
            });
        if let Err(e) = spawned {
            tracing::debug!(error = %e, "couldn't start the notification thread");
        }
    }
}

/// The part that notices what waits on you and rings for it.
pub struct NotificationsPart {
    config: watch::Receiver<Config>,
    sink: Arc<dyn NotificationSink>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The first snapshot has been seen.
    baseline: bool,
    /// Every waiting line already noticed.
    seen: HashSet<MessageId>,
    /// How many times the bell has rung since launch.
    bell: u64,
}

impl fmt::Debug for NotificationsPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NotificationsPart")
            .field("sink", &self.sink)
            .finish_non_exhaustive()
    }
}

impl NotificationsPart {
    /// Reads whether notifications are on from `config` at each snapshot, so a change to the
    /// setting applies from the next one.
    pub fn new(config: watch::Receiver<Config>, sink: Arc<dyn NotificationSink>) -> Self {
        Self {
            config,
            sink,
            state: Mutex::new(State::default()),
        }
    }

    /// Notices the snapshot's waiting lines; returns the notifications to send and sets the
    /// snapshot's bell count.
    fn observe(&self, snapshot: &mut Snapshot) -> Vec<Notification> {
        let enabled = self.config.borrow().notifications.enabled;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let fresh: Vec<Notification> = snapshot
            .conversation
            .iter()
            .filter_map(waiting)
            .filter(|n| state.seen.insert(n.message))
            .collect();
        let announce = std::mem::replace(&mut state.baseline, true) && enabled;
        let out = if announce && !fresh.is_empty() {
            state.bell += 1;
            fresh
        } else {
            Vec::new()
        };
        snapshot.bell = state.bell;
        out
    }
}

impl ApiPart for NotificationsPart {
    fn handle<'a>(&'a self, _command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        None
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for notification in self.observe(snapshot) {
                self.sink.notify(&notification);
            }
        })
    }
}

/// The notification for a line that waits on you, or `None` for ordinary chat.
pub fn waiting(line: &ChatLine) -> Option<Notification> {
    let kind = match &line.card {
        Some(card) if card.answered.is_some() => return None,
        Some(card) => match card.kind {
            CardKind::Approval => AttentionKind::Approval,
            CardKind::Decision => AttentionKind::Decision,
            CardKind::Question => AttentionKind::Question,
            CardKind::Plan => match card.heading.as_str() {
                crate::plans::api::QUESTIONS_HEADING => AttentionKind::Question,
                crate::plans::api::FINAL_HEADING => AttentionKind::Approval,
                _ => return None,
            },
            _ => return None,
        },
        None if line.kind == MessageKind::Question => AttentionKind::Question,
        None => return None,
    };
    let body = match (&line.card, kind) {
        (Some(card), AttentionKind::Approval | AttentionKind::Decision) => match &card.title {
            Some(title) => format!("{} · {title}", card.heading),
            None => card.heading.clone(),
        },
        _ => line.text.clone(),
    };
    Some(Notification {
        kind,
        message: line.id,
        title: TITLE.to_owned(),
        subtitle: kind.label().to_owned(),
        body: one_line(&body, MAX_BODY_CHARS),
    })
}

/// `text` on one line, control characters dropped, cut to `max` characters with "…".
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}
