//! Time behind one trait, for limits and watchdog ladders.
//!
//! A [`Clock`] gives wall time ([`jiff::Timestamp`], UTC, for records and schedules) and monotonic
//! time ([`Instant`], for deadlines and elapsed time), and sleeps until a monotonic deadline.
//! Production code takes `&dyn Clock` (or `Arc<dyn Clock>`) and never calls `Instant::now()`,
//! `SystemTime::now()` or `tokio::time` directly.
//!
//! [`SystemClock`] is the real one (its sleeps need a Tokio runtime).
//!
//! **Sleep.** On macOS the monotonic clock stops while the Mac sleeps, so an `Instant` deadline
//! slides by however long the lid was shut. Durations measured while the app runs (limits, the stop
//! sequence) use [`Clock::instant`]; anything due at a time of day (schedules) re-checks
//! [`Clock::now`] on waking instead of trusting one long sleep.

use crate::BoxFuture;
use jiff::Timestamp;
use std::fmt;
use std::time::{Duration, Instant};

/// A sleep returned by [`Clock::sleep_until`]: completes once the clock reaches the deadline.
pub type Sleep = BoxFuture<'static, ()>;

/// Wall and monotonic time, and sleeping.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current wall-clock time, UTC.
    fn now(&self) -> Timestamp;

    /// The current monotonic time.
    fn instant(&self) -> Instant;

    /// Completes when [`Clock::instant`] reaches `deadline`; at once if it already has.
    fn sleep_until(&self, deadline: Instant) -> Sleep;

    /// Completes after `duration` of this clock's time.
    fn sleep(&self, duration: Duration) -> Sleep {
        self.sleep_until(self.instant() + duration)
    }
}

/// The real clock. Sleeps run on the Tokio runtime's timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
            deadline,
        )))
    }
}
