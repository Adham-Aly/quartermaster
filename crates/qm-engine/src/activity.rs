//! Coalesced activity reports.
//!
//! A long thinking stretch or a long-running built-in tool produces no committed event for minutes,
//! which the core's mid-turn quiet watchdog would read as silence. Engines report such work as
//! [`crate::EngineEvent::Activity`], at most once per [`ACTIVITY_EVERY`] of silence, through an
//! [`ActivityGate`], so both engines report it the same way and never flood the stream.

use std::time::{Duration, Instant};

/// The least time between two events when the only news is that work goes on.
pub const ACTIVITY_EVERY: Duration = Duration::from_secs(30);

/// Decides when silent work is worth an `Activity` event. Pure: time comes in as arguments.
///
/// The engine calls [`ActivityGate::sent`] for every event it sends and [`ActivityGate::due`]
/// whenever it sees work with nothing to commit (a thinking delta, a running tool's progress);
/// it sends `Activity` only when `due` says so.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActivityGate {
    last_sent: Option<Instant>,
}

impl ActivityGate {
    pub const fn new() -> Self {
        Self { last_sent: None }
    }

    /// The engine sent an event now: any event shows the session is alive.
    pub fn sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }

    /// Whether silent work seen now should be reported: true once at least [`ACTIVITY_EVERY`]
    /// has passed since the last event sent (or nothing was sent yet). When it returns true it
    /// counts the `Activity` as sent.
    pub fn due(&mut self, now: Instant) -> bool {
        let due = self
            .last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= ACTIVITY_EVERY);
        if due {
            self.last_sent = Some(now);
        }
        due
    }
}
