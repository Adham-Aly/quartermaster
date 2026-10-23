//! Each agent's context occupancy, current turn and compactions, display-ready.
//!
//! The core fills these from passive engine readings it keeps in memory. Nothing here is a model
//! input, and nothing here outlives the server.

use super::AgentRef;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// One agent's context, as the strip above its input and its task card show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentContextView {
    pub agent: AgentRef,
    pub occupancy: ContextOccupancy,
    /// The running turn so far, else the latest finished turn; `None` before any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnTokens>,
    /// The engine is compacting right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting: Option<CompactingView>,
    /// The latest compactions that ended, oldest first (at most three).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<CompactionNote>,
}

impl AgentContextView {
    pub fn new(agent: AgentRef, occupancy: ContextOccupancy) -> Self {
        Self {
            agent,
            occupancy,
            turn: None,
            compacting: None,
            notes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_turn(mut self, turn: Option<TurnTokens>) -> Self {
        self.turn = turn;
        self
    }

    #[must_use]
    pub fn with_compacting(mut self, compacting: Option<CompactingView>) -> Self {
        self.compacting = compacting;
        self
    }

    #[must_use]
    pub fn with_notes(mut self, notes: Vec<CompactionNote>) -> Self {
        self.notes = notes;
        self
    }
}

/// How full the context is against the point where the engine compacts by itself. Never a
/// cumulative sum: `tokens` is the latest single model call's prompt plus reply, or the engine's
/// own figure at session start or after a compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ContextOccupancy {
    pub tokens: Option<u64>,
    /// Estimated rather than counted (shown with "~").
    pub estimate: bool,
    pub point: PointView,
    pub window: Option<u64>,
    pub model: Option<String>,
    /// When the reading was taken; `None` before any reading.
    pub observed_at: Option<Timestamp>,
    pub basis: ContextBasisView,
    /// Why the reading may no longer hold ("session closed").
    pub stale: Option<String>,
    /// Why `tokens` is unknown ("waiting for the first model call").
    pub unknown_why: Option<String>,
}

impl ContextOccupancy {
    /// No reading at all, for this reason.
    pub fn unknown(why: impl Into<String>) -> Self {
        Self {
            unknown_why: Some(why.into()),
            ..Self::default()
        }
    }
}

/// Where the engine compacts by itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "point", content = "value", rename_all = "snake_case")]
pub enum PointView {
    At(u64),
    Off,
    Unknown(String),
    /// No reading has said.
    #[default]
    NotReported,
}

/// What the occupancy figure measures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBasisView {
    #[default]
    None,
    /// The latest model call.
    LastCall,
    /// The engine's figure when the session started or resumed.
    SessionStart,
    /// The engine's figure right after a compaction.
    AfterCompaction,
}

/// One turn's usage by category. A category is `Some` only when the engine reports it separately
/// or its value is non-zero, so an unsupported one is left out rather than shown as zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct TurnTokens {
    /// The turn is still running ("this turn so far"), else it's the latest finished one.
    pub running: bool,
    /// Uncached input only.
    pub input: u64,
    /// Output, thinking included.
    pub output: u64,
    pub thinking: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// Formatted ("$0.04"); only once the turn has ended, and only where the engine reports it.
    pub cost: Option<String>,
    pub model_calls: u32,
    /// When it started (running) or ended (finished).
    pub at: Option<Timestamp>,
}

impl TurnTokens {
    pub fn new(running: bool, input: u64, output: u64, model_calls: u32) -> Self {
        Self {
            running,
            input,
            output,
            model_calls,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_categories(
        mut self,
        thinking: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
    ) -> Self {
        self.thinking = thinking;
        self.cache_read = cache_read;
        self.cache_write = cache_write;
        self
    }

    #[must_use]
    pub fn with_cost(mut self, cost: Option<String>) -> Self {
        self.cost = cost;
        self
    }

    #[must_use]
    pub fn with_at(mut self, at: Option<Timestamp>) -> Self {
        self.at = at;
        self
    }
}

/// A compaction in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompactingView {
    pub automatic: bool,
    pub since: Timestamp,
    /// The occupancy just before it started, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<u64>,
}

impl CompactingView {
    pub fn new(automatic: bool, since: Timestamp, was: Option<u64>) -> Self {
        Self {
            automatic,
            since,
            was,
        }
    }
}

/// A compaction that ended, anchored in the chat at `at`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompactionNote {
    pub at: Timestamp,
    pub automatic: bool,
    pub outcome: NoteOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    #[serde(default)]
    pub after_estimate: bool,
    /// How long it took, when its start was seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub took_ms: Option<u64>,
}

impl CompactionNote {
    pub fn new(at: Timestamp, automatic: bool, outcome: NoteOutcome) -> Self {
        Self {
            at,
            automatic,
            outcome,
            before: None,
            after: None,
            after_estimate: false,
            took_ms: None,
        }
    }

    #[must_use]
    pub fn with_sizes(mut self, before: Option<u64>, after: Option<u64>, estimate: bool) -> Self {
        self.before = before;
        self.after = after;
        self.after_estimate = estimate;
        self
    }

    #[must_use]
    pub fn with_took_ms(mut self, took_ms: Option<u64>) -> Self {
        self.took_ms = took_ms;
        self
    }
}

/// How a compaction ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "message", rename_all = "snake_case")]
pub enum NoteOutcome {
    Done,
    Failed(String),
}

/// A token count, short: `950`, `9.1k`, `182k`, `1.2M`, `34M`.
pub fn tokens_short(n: u64) -> String {
    // Tenths, rounded, without floating point.
    let short = |n: u64, unit: u64, suffix: &str| {
        let tenths = (n.saturating_mul(10) + unit / 2) / unit;
        if tenths < 100 {
            format!("{}.{}{suffix}", tenths / 10, tenths % 10)
        } else {
            format!("{}{suffix}", (n + unit / 2) / unit)
        }
    };
    match n {
        0..1_000 => n.to_string(),
        1_000..999_500 => short(n, 1_000, "k"),
        _ => short(n, 1_000_000, "M"),
    }
}
