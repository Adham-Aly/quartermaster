//! Applying `SessionSpec::limits` inside the request loop.
//!
//! Only two limits live in the engine, because only the engine sees each request: the
//! orchestrators' output cap per message (thinking included) and the retries per model call. The
//! rest (calls per turn, the call and turn clocks) are the core's, read from our events by its
//! `TurnMeter`. Crewmates have no limits of ours: `limits` is `None`, and the provider's own two
//! retries apply.

use qm_engine::{CallUsage, FailureKind, SessionLimits, StopReason, TurnFailure};

/// The provider's retries per call when the spec sets none.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// The limits one session runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Retries per model call before the turn fails.
    pub max_retries: u32,
    /// The output cap per message, thinking included (orchestrators only).
    pub output_cap: Option<u32>,
}

impl Limits {
    pub fn from_spec(limits: Option<SessionLimits>) -> Self {
        limits.map_or(
            Self {
                max_retries: DEFAULT_MAX_RETRIES,
                output_cap: None,
            },
            |l| Self {
                max_retries: l.max_retries,
                output_cap: Some(l.max_output_tokens),
            },
        )
    }

    /// The failure when a reply passed the cap, or reached it and was cut off there. Sent or not,
    /// the cap is checked after every call: Sign in with ChatGPT can't carry it.
    pub fn cap_failure(&self, usage: &CallUsage, stop: StopReason) -> Option<TurnFailure> {
        let cap = u64::from(self.output_cap?);
        let passed = usage.output_tokens > cap;
        let cut_at_cap = stop == StopReason::MaxTokens && usage.output_tokens >= cap;
        (passed || cut_at_cap).then(|| TurnFailure {
            kind: FailureKind::Provider,
            message: cap_message(self.output_cap.unwrap_or_default()),
        })
    }
}

/// "the reply passed the 8,192-token output cap (thinking included)".
pub fn cap_message(cap: u32) -> String {
    format!(
        "the reply passed the {}-token output cap (thinking included)",
        thousands(u64::from(cap))
    )
}

/// `8192` → `8,192`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
