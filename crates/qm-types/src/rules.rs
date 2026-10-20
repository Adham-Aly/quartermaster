//! The facts the rules in `qm-policy` judge, shared with the crates that produce them: a turn's
//! counters (`qm-engine`), a tool's class (`qm-tools`) and why a task is held (`qm-store`). One
//! definition each, so no crate keeps a mirror.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A turn's measured facts at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnCounters {
    /// Model calls started so far in this turn.
    pub model_calls: u32,
    /// The current call's clock: time since the current model call started (or since the turn
    /// started, before the first call is reported), not counting time while our tools ran.
    pub call_elapsed: Duration,
    /// Time since the turn started. Never paused: it is the backstop.
    pub turn_elapsed: Duration,
    /// The most output tokens (thinking included) any single message in this turn produced.
    pub largest_message_output_tokens: u64,
    /// Output tokens across the turn, thinking included.
    pub output_tokens: u64,
}

/// Whether a successful call counts as an effective action, and which calls still work in the
/// action rule's retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolClass {
    /// Shown to you: `reply`, `ask_user`, `acknowledge`. Counts, and is all that works in the retry
    /// of a turn that did nothing.
    Speak,
    /// Starts, steers, stops or settles work, or changes a task, setting or schedule. Counts.
    Act,
    /// A lookup. Never counts.
    Read,
}

impl ToolClass {
    /// Whether a successful call satisfies the effective-action rule.
    pub const fn counts(self) -> bool {
        matches!(self, Self::Speak | Self::Act)
    }
}

/// Why the app holds a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// The third failed repair of its own regression.
    RepairLimit,
    /// A third "findings" verdict after two re-reviews.
    ReviewRoundLimit,
    /// The idle ladder's last step: the "stuck" notice.
    Stuck,
    /// A blocked crewmate whose next move only you can make.
    NeedsUser,
}
