//! Every string that crosses into an agent's input, in one place.
//!
//! Three sources an agent tells apart: its supervisor (marked), the app (marked) and you
//! (unmarked, and authoritative). Only the app produces markers, so:
//!
//! - [`user_text`] is the one way your words become a [`UserText`]: neutralised, so nothing you
//!   type can open or close a marker. The TUI's `Send` to any agent goes through it.
//! - [`external`] neutralises and cuts text that came from outside the app (CI logs, GitHub text,
//!   review findings, a supervisor's summary) before it is embedded anywhere.
//!
//! The neutralising itself is `qm_policy::markers::neutralise` (`<` of a reserved prefix becomes
//! `‹`).

use qm_policy::markers::neutralise;
use std::fmt;

/// Your words, neutralised. Only [`user_text`] builds one.
#[derive(Clone, PartialEq, Eq)]
pub struct UserText(String);

impl UserText {
    /// The text as it reaches the agent.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Nothing but whitespace: ignored.
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for UserText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("UserText").field(&self.0).finish()
    }
}

impl fmt::Display for UserText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Your words as an agent receives them: surrounding whitespace trimmed, every reserved prefix
/// neutralised.
pub fn user_text(raw: &str) -> UserText {
    UserText(neutralise(raw.trim()))
}

/// Text from outside the app, neutralised and cut to `cap` characters (ending in `…` when cut).
pub fn external(raw: &str, cap: usize) -> String {
    let text = neutralise(raw);
    if text.chars().count() <= cap {
        return text;
    }
    let mut out: String = text.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}
