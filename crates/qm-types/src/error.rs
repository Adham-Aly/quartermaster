//! Errors shared across crates.
//!
//! The workspace's error policy: every library crate has its own `thiserror` enum for what can go
//! wrong inside it; `anyhow` is used only in the binary and `xtask`. A rule saying "no" is not an
//! error but a [`Refusal`]: a plain sentence the agent can read and act on. No error or refusal
//! ever carries a secret.

use serde::{Deserialize, Serialize};

/// A rule refused something, with a plain reason an agent can read.
///
/// Refusals and failed calls never count as effective actions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
#[error("{reason}")]
pub struct Refusal {
    /// One plain sentence, written for the agent, e.g. "a named model needs a turn started by the
    /// user's own message".
    pub reason: String,
}

impl Refusal {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// A string that isn't a valid ID of the expected kind.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{input:?} is not a valid {kind}: {why}")]
pub struct IdError {
    pub kind: &'static str,
    pub input: String,
    pub why: &'static str,
}

impl IdError {
    pub(crate) fn new(kind: &'static str, input: &str, why: &'static str) -> Self {
        Self {
            kind,
            input: input.to_owned(),
            why,
        }
    }
}

/// A string that isn't one of an enum's wire names.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a valid {kind}; expected one of: {expected}")]
pub struct UnknownVariant {
    pub kind: &'static str,
    pub value: String,
    pub expected: String,
}
