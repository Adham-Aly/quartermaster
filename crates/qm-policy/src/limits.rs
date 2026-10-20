//! Limits on the orchestrators' turns.
//!
//! | Limit | Quartermaster and secondmates | Crewmates |
//! |---|---|---|
//! | Time per model call | 5 min, with the clock paused while tools run | none of ours |
//! | Time per turn | 20 min, as a backstop | none |
//! | Model calls per turn | 40 | none |
//! | Output tokens per message | 8,192, thinking included | none |
//! | Retries per model call | 2 | the provider's 2 retries |
//! | When a limit is hit | the turn fails with a plain reason | stuck crewmates are caught by the watchdogs instead |
//!
//! "Limits bound only the orchestrators' turns." The engine layer counts (`qm_engine::TurnMeter`);
//! this module checks the [`TurnCounters`], which live in `qm-types`.

use qm_types::domain::Role;
use qm_types::rules::TurnCounters;
use std::time::Duration;

/// 5 minutes per model call, paused while our tools run.
pub const MAX_CALL_TIME: Duration = Duration::from_mins(5);
/// 20 minutes per turn, never paused.
pub const MAX_TURN_TIME: Duration = Duration::from_mins(20);
/// 40 model calls per turn.
pub const MAX_MODEL_CALLS: u32 = 40;
/// 8,192 output tokens per message, thinking included.
pub const MAX_OUTPUT_TOKENS: u32 = 8_192;
/// 2 retries per model call.
pub const MAX_RETRIES: u32 = 2;

/// The limits an engine applies inside its own request loop (`SessionSpec::limits`), for the
/// orchestrators only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineLimits {
    pub max_output_tokens: u32,
    pub max_retries: u32,
}

/// Whether a role's turns are limited at all.
pub const fn is_limited(role: Role) -> bool {
    matches!(role, Role::Quartermaster | Role::Secondmate)
}

/// What travels in `SessionSpec::limits`: `Some` for orchestrators, `None` for crewmates.
pub const fn engine_limits(role: Role) -> Option<EngineLimits> {
    if is_limited(role) {
        Some(EngineLimits {
            max_output_tokens: MAX_OUTPUT_TOKENS,
            max_retries: MAX_RETRIES,
        })
    } else {
        None
    }
}

/// A limit that was hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitHit {
    CallTime,
    TurnTime,
    ModelCalls,
    OutputTokens,
}

impl LimitHit {
    /// "The turn fails with a plain reason"; for the output cap, "with the cap named as the
    /// reason".
    pub const fn reason(self) -> &'static str {
        match self {
            Self::CallTime => "a model call ran past the 5-minute limit",
            Self::TurnTime => "the turn ran past the 20-minute limit",
            Self::ModelCalls => "the turn reached the limit of 40 model calls",
            Self::OutputTokens => {
                "a message reached the 8,192-token output cap (thinking included)"
            }
        }
    }
}

/// Checks a turn's counters. Crewmates are never limited.
pub fn check(role: Role, c: &TurnCounters) -> Result<(), LimitHit> {
    if !is_limited(role) {
        return Ok(());
    }
    if c.largest_message_output_tokens >= u64::from(MAX_OUTPUT_TOKENS) {
        return Err(LimitHit::OutputTokens);
    }
    if c.model_calls > MAX_MODEL_CALLS {
        return Err(LimitHit::ModelCalls);
    }
    if c.call_elapsed >= MAX_CALL_TIME {
        return Err(LimitHit::CallTime);
    }
    if c.turn_elapsed >= MAX_TURN_TIME {
        return Err(LimitHit::TurnTime);
    }
    Ok(())
}

/// Whether one more model call may start: the 41st is refused.
pub fn may_start_call(role: Role, calls_so_far: u32) -> bool {
    !is_limited(role) || calls_so_far < MAX_MODEL_CALLS
}
