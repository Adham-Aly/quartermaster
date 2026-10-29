//! The Actions inbox: every request waiting on you, built fresh by the core for each snapshot.

use super::{AgentRef, Card};
use crate::ids::{AccountId, HeldActionId, MessageId, TaskId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// One request waiting on you. Built by the core; the UI only draws it and sends its buttons.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ActionItem {
    pub key: ActionKey,
    pub kind: ActionKind,
    /// Who asked: where a Reply goes. `None` for the app's own prompts (setup, models, accounts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<AgentRef>,
    /// Display-ready source name, retained even when a closed task has left the crew tree.
    /// Routing still uses `from` and `task`, never these words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// One line: "Merge PR #41 · feat(billing): proration preview".
    pub title: String,
    /// When it started waiting on you.
    pub at: Timestamp,
    /// The item's card, buttons included: the only place a card is shown (in the Actions popup).
    pub card: Card,
    /// The item's own line in the Quartermaster's chat: a question's or a notice's message.
    /// Held actions and plans have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<MessageId>,
    /// Where "Go to" takes you.
    pub open: ActionOpen,
}

impl ActionItem {
    pub fn new(
        key: ActionKey,
        kind: ActionKind,
        title: impl Into<String>,
        at: Timestamp,
        card: Card,
        open: ActionOpen,
    ) -> Self {
        Self {
            key,
            kind,
            from: None,
            from_label: None,
            task: None,
            title: title.into(),
            at,
            card,
            anchor: None,
            open,
        }
    }

    #[must_use]
    pub fn with_from(mut self, from: AgentRef) -> Self {
        self.from = Some(from);
        self
    }

    #[must_use]
    pub fn with_from_label(mut self, label: impl Into<String>) -> Self {
        self.from_label = Some(label.into());
        self
    }

    #[must_use]
    pub fn with_task(mut self, task: TaskId) -> Self {
        self.task = Some(task);
        self
    }

    #[must_use]
    pub fn with_anchor(mut self, anchor: MessageId) -> Self {
        self.anchor = Some(anchor);
        self
    }

    /// The app's own configuration prompts, which block everything else and so sort first.
    pub fn is_configuration(&self) -> bool {
        matches!(
            self.key,
            ActionKey::ModelLevels | ActionKey::Setup { .. } | ActionKey::Account { .. }
        )
    }
}

/// What an item is, stable across snapshots: the UI's selection follows it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionKey {
    /// A pending held action (approval or decision).
    Held { id: HeldActionId },
    /// A plan task waiting on your reply or your approval.
    Plan { task: TaskId },
    /// A review task's report waiting on your go-ahead.
    Review { task: TaskId },
    /// A finished answer task's report (lookup, research, reproduce, teach) you haven't dismissed.
    Report { task: TaskId },
    /// An agent's question, by its message.
    Question { message: MessageId },
    /// A no-action notice, by its message.
    Notice { message: MessageId },
    /// No model level is configured yet.
    ModelLevels,
    /// One of the setup check's findings, by its text.
    Setup { issue: String },
    /// An account that needs you to log in again.
    Account { id: AccountId },
}

/// What kind of request: its glyph and colour in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionKind {
    Approval,
    Decision,
    Question,
    Plan,
    Notice,
    Setup,
    Review,
    Report,
}

/// Where an item's "Go to" takes you.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionOpen {
    /// The Quartermaster's conversation: such an item has no "Go to"; its popup is where it's
    /// answered.
    Conversation,
    /// A worker's session.
    Session {
        agent: AgentRef,
    },
    Settings,
    Setup,
    /// The plan's page.
    Plan {
        task: TaskId,
    },
}
