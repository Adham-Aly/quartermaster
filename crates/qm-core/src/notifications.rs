//! Getting your attention: the terminal bell and a macOS desktop notification, only for questions,
//! decisions and approvals, and never when notifications are switched off (`[notifications] enabled
//! = false` in `config.toml`).
//!
//! [`NotificationsPart`] is an [`ApiPart`] filled **after every other part**, so it reads the
//! Actions list ([`Snapshot::actions`]) exactly as the Bridge will draw it. Every item that
//! notifies (an approval, a decision, a question from the Quartermaster, a plan waiting on your
//! answers or approval) is noticed once, by its key and the time it started waiting, the first
//! time a snapshot carries it; a plan's new round has a new time, so it notifies again. Secondmate
//! questions, notices and the app's setup prompts never notify.
//!
//! For each new one, when notifications are on, the part
//! - raises [`Snapshot::bell`] by one; the UI writes BEL to the terminal when it rises; and
//! - hands a [`Notification`] to its [`NotificationSink`], which on the Mac is
//!   [`DesktopNotifier`] (`osascript`'s `display notification`). Tests give it a fake sink.
//!
//! The first snapshot after launch is the baseline: what was already waiting when the app
//! started is shown, not announced again.

use crate::api::{ApiPart, Handled};
use jiff::Timestamp;
use qm_types::BoxFuture;
use qm_types::api::{ActionItem, ActionKey, ActionKind, AgentRef, Command, Snapshot};
use qm_types::config::Config;
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
    /// A question from the Quartermaster's `ask_user`, or a plan's questions.
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
    /// The Actions item it is about.
    pub key: ActionKey,
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
    /// Every waiting item already noticed, with the time it started waiting.
    seen: HashSet<(ActionKey, Timestamp)>,
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

    /// Notices the snapshot's waiting items; returns the notifications to send and sets the
    /// snapshot's bell count.
    fn observe(&self, snapshot: &mut Snapshot) -> Vec<Notification> {
        let enabled = self.config.borrow().notifications.enabled;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let fresh: Vec<Notification> = snapshot
            .actions
            .iter()
            .filter(|item| state.seen.insert((item.key.clone(), item.at)))
            .filter_map(waiting)
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

/// The notification for an Actions item, or `None` for one that doesn't notify.
pub fn waiting(item: &ActionItem) -> Option<Notification> {
    let card = &item.card;
    let (kind, body) = match item.kind {
        ActionKind::Approval | ActionKind::Decision => {
            let kind = if item.kind == ActionKind::Approval {
                AttentionKind::Approval
            } else {
                AttentionKind::Decision
            };
            let body = match &card.title {
                Some(title) => format!("{} · {title}", card.heading),
                None => card.heading.clone(),
            };
            (kind, body)
        }
        // The question's card carries the full question as its first line.
        ActionKind::Question if item.from == Some(AgentRef::Quartermaster) => {
            let body = card.lines.first().map_or_else(
                || item.title.clone(),
                |line| line.spans.iter().map(|s| s.text.as_str()).collect(),
            );
            (AttentionKind::Question, body)
        }
        ActionKind::Plan => match card.heading.as_str() {
            crate::plans::api::QUESTIONS_HEADING => (AttentionKind::Question, item.title.clone()),
            crate::plans::api::FINAL_HEADING => (AttentionKind::Approval, item.title.clone()),
            _ => return None,
        },
        _ => return None,
    };
    Some(Notification {
        kind,
        key: item.key.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::api::{FINAL_HEADING, QUESTIONS_HEADING};
    use qm_types::api::{ActionOpen, Card, CardKind, CardLine, Styled, Tone};
    use qm_types::ids::{HeldActionId, MessageId, SecondmateId, TaskId};

    #[derive(Debug, Default)]
    struct Sink(Mutex<Vec<Notification>>);

    impl NotificationSink for Sink {
        fn notify(&self, notification: &Notification) {
            self.0.lock().unwrap().push(notification.clone());
        }
    }

    fn part(enabled: bool) -> (NotificationsPart, Arc<Sink>, watch::Sender<Config>) {
        let mut config = Config::default();
        config.notifications.enabled = enabled;
        let (tx, rx) = watch::channel(config);
        let sink = Arc::new(Sink::default());
        (
            NotificationsPart::new(rx, Arc::clone(&sink) as Arc<dyn NotificationSink>),
            sink,
            tx,
        )
    }

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).unwrap()
    }

    fn held(id: i64, kind: CardKind) -> ActionItem {
        let mut card = Card::new(kind, "Approval needed · T-0042");
        card.title = Some("feat(billing): proration".to_owned());
        let action_kind = if kind == CardKind::Decision {
            ActionKind::Decision
        } else {
            ActionKind::Approval
        };
        ActionItem::new(
            ActionKey::Held {
                id: HeldActionId::new(id),
            },
            action_kind,
            "t",
            at(1),
            card,
            ActionOpen::Conversation,
        )
    }

    fn question(id: i64, from: AgentRef) -> ActionItem {
        let mut card = Card::new(CardKind::Question, "Question");
        card.lines = vec![CardLine::new(
            2,
            vec![Styled::new("Which cache?\nThe session one?", Tone::Text)],
        )];
        ActionItem::new(
            ActionKey::Question {
                message: MessageId::new(id),
            },
            ActionKind::Question,
            "Which cache?",
            at(1),
            card,
            ActionOpen::Conversation,
        )
        .with_from(from)
    }

    fn plan(heading: &str, second: i64) -> ActionItem {
        let task = TaskId::new(44).unwrap();
        ActionItem::new(
            ActionKey::Plan { task },
            ActionKind::Plan,
            format!("{heading} · plan(auth)"),
            at(second),
            Card::new(CardKind::Plan, heading),
            ActionOpen::Plan { task },
        )
    }

    fn notice(id: i64) -> ActionItem {
        ActionItem::new(
            ActionKey::Notice {
                message: MessageId::new(id),
            },
            ActionKind::Notice,
            "n",
            at(1),
            Card::new(CardKind::Notice, "Nothing came of your message"),
            ActionOpen::Conversation,
        )
    }

    fn run(part: &NotificationsPart, items: Vec<ActionItem>) -> (Vec<Notification>, u64) {
        let mut snapshot = Snapshot::default();
        snapshot.actions = items;
        let out = part.observe(&mut snapshot);
        (out, snapshot.bell)
    }

    #[test]
    fn each_notifying_kind_alerts_once_with_todays_text() {
        let (part, _, _) = part(true);
        assert_eq!(run(&part, Vec::new()).0, Vec::new());
        let items = vec![
            held(1, CardKind::Approval),
            held(2, CardKind::Decision),
            question(3, AgentRef::Quartermaster),
            plan(QUESTIONS_HEADING, 5),
        ];
        let (out, bell) = run(&part, items.clone());
        assert_eq!(bell, 1);
        let got: Vec<_> = out
            .iter()
            .map(|n| (n.subtitle.as_str(), n.body.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "Approval needed",
                    "Approval needed · T-0042 · feat(billing): proration"
                ),
                (
                    "Decision",
                    "Approval needed · T-0042 · feat(billing): proration"
                ),
                ("Question", "Which cache? The session one?"),
                ("Question", "Plan questions · plan(auth)"),
            ]
        );
        assert!(out.iter().all(|n| n.title == TITLE));
        // Unchanged items never alert twice.
        let (out, bell) = run(&part, items);
        assert_eq!(out, Vec::new());
        assert_eq!(bell, 1);
    }

    #[test]
    fn secondmate_questions_notices_and_setup_never_alert() {
        let (part, _, _) = part(true);
        run(&part, Vec::new());
        let setup = ActionItem::new(
            ActionKey::ModelLevels,
            ActionKind::Setup,
            "Choose model levels",
            at(1),
            Card::new(CardKind::Notice, "Choose model levels"),
            ActionOpen::Settings,
        );
        let mate = AgentRef::Secondmate {
            id: SecondmateId::new(2).unwrap(),
        };
        let (out, bell) = run(&part, vec![question(1, mate), notice(2), setup]);
        assert_eq!(out, Vec::new());
        assert_eq!(bell, 0);
    }

    #[test]
    fn the_first_snapshot_is_the_baseline() {
        let (part, _, _) = part(true);
        let (out, bell) = run(&part, vec![held(1, CardKind::Approval)]);
        assert_eq!(out, Vec::new());
        assert_eq!(bell, 0);
        assert_eq!(run(&part, vec![held(1, CardKind::Approval)]).0, Vec::new());
    }

    #[test]
    fn a_new_plan_round_alerts_again() {
        let (part, _, _) = part(true);
        run(&part, Vec::new());
        assert_eq!(run(&part, vec![plan(QUESTIONS_HEADING, 5)]).0.len(), 1);
        assert_eq!(run(&part, vec![plan(QUESTIONS_HEADING, 5)]).0, Vec::new());
        let (out, bell) = run(&part, vec![plan(FINAL_HEADING, 9)]);
        assert_eq!(out[0].subtitle, "Approval needed");
        assert_eq!(bell, 2);
    }

    #[test]
    fn switched_off_nothing_alerts() {
        let (part, _, _) = part(false);
        run(&part, Vec::new());
        let (out, bell) = run(&part, vec![held(1, CardKind::Approval)]);
        assert_eq!(out, Vec::new());
        assert_eq!(bell, 0);
    }
}
