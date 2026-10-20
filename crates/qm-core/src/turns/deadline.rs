//! When the next limit can come due.
//!
//! The per-call clock runs only while the model works: it is paused from a tool call until its
//! result is sent back (`TurnMeter`), so a long tool never counts against the 5 minutes. The turn
//! clock never pauses: 20 minutes from the moment the turn was asked for.

use qm_policy::limits::{MAX_CALL_TIME, MAX_TURN_TIME};
use qm_types::rules::TurnCounters;
use std::time::Instant;

/// The earliest instant at which a time limit could be reached, given the counters at `now`.
/// With the call clock paused only the turn's backstop can come due.
pub fn next_deadline(counters: &TurnCounters, call_clock_running: bool, now: Instant) -> Instant {
    let turn = now + MAX_TURN_TIME.saturating_sub(counters.turn_elapsed);
    if call_clock_running {
        turn.min(now + MAX_CALL_TIME.saturating_sub(counters.call_elapsed))
    } else {
        turn
    }
}
