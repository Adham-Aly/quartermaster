//! Passive readings of how full a session's context is, for the UI.
//!
//! An engine builds these from messages it already receives and numbers it already computes. A
//! reading never changes what the session does: no engine asks for one, and nothing it holds
//! reaches a model.

use serde::{Deserialize, Serialize};

/// How full the context is, as the engine last saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextReading {
    /// Tokens in the context: the latest call's prompt plus its reply. `None` means unknown.
    pub tokens: Option<u64>,
    pub basis: ContextBasis,
    /// The engine estimated rather than counted (shown with "~").
    pub estimate: bool,
    /// Where this engine compacts by itself.
    pub autocompact: AutocompactPoint,
    /// The model's context window, when known.
    pub window: Option<u64>,
    pub model: Option<String>,
    /// The usage categories this engine reports separately (input and output always are).
    pub reports: ReportedUsage,
}

impl ContextReading {
    pub fn new(
        tokens: Option<u64>,
        basis: ContextBasis,
        estimate: bool,
        autocompact: AutocompactPoint,
    ) -> Self {
        Self {
            tokens,
            basis,
            estimate,
            autocompact,
            window: None,
            model: None,
            reports: ReportedUsage::default(),
        }
    }

    #[must_use]
    pub fn with_window(mut self, window: Option<u64>) -> Self {
        self.window = window;
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    #[must_use]
    pub fn with_reports(mut self, reports: ReportedUsage) -> Self {
        self.reports = reports;
        self
    }
}

/// What a reading's token count measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBasis {
    /// The latest main-thread model call: its prompt plus its reply.
    LastCall,
    /// The engine's own figure when the session started or resumed.
    SessionStart,
}

/// The context size at which the engine compacts by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "point", rename_all = "snake_case")]
pub enum AutocompactPoint {
    At { tokens: u64 },
    Off,
    Unknown { why: String },
}

/// Which usage categories an engine reports separately.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReportedUsage {
    pub thinking: bool,
    pub cache_read: bool,
    pub cache_write: bool,
    pub cost: bool,
}

impl ReportedUsage {
    pub const fn new(thinking: bool, cache_read: bool, cache_write: bool, cost: bool) -> Self {
        Self {
            thinking,
            cache_read,
            cache_write,
            cost,
        }
    }
}
