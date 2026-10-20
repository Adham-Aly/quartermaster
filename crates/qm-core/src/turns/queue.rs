//! Step 1 of a turn: what arrived, and what runs next.
//!
//! Pure: time is an argument, so the batching rules run on any clock.
//!
//! - **Your messages** are a FIFO, one turn each. While a turn runs they wait, shown as queued.
//! - **Wakes** carry no data here: a signal only says a wake for the seat was written (the wake
//!   itself is in the `wakes` table). Wakes are ready once none has arrived for
//!   [`WAKE_QUIET`] (so wakes within about two seconds of each other go together), or once the
//!   first has waited [`WAKE_MAX_HOLD`] (so a steady trickle can't hold a turn off forever).
//! - **Order:** a waiting message always goes first and takes every pending wake with it; a wake
//!   turn starts only when no message of yours is waiting.

use super::{WAKE_MAX_HOLD, WAKE_QUIET};
use qm_types::ids::MessageId;
use std::collections::VecDeque;
use std::time::Instant;

/// What the loop should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Start a turn for this message of yours (it takes every pending wake along).
    User(MessageId),
    /// Start a wake turn.
    Wakes,
    /// Wakes are waiting for their batch to close: look again at this instant.
    WaitUntil(Instant),
    /// Nothing to do.
    Idle,
}

/// Your queued messages and the wake window.
#[derive(Clone, Debug, Default)]
pub struct InputQueue {
    users: VecDeque<MessageId>,
    /// When the first and the last wake signal not yet taken by a turn arrived.
    first_wake: Option<Instant>,
    last_wake: Option<Instant>,
    /// Startup catch-up: pending wakes are ready at once.
    catch_up: bool,
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Your message arrived (or "Ask again" on a notice): it gets its own turn. A message already
    /// waiting here isn't queued twice (startup recovery and its own signal both name it).
    pub fn push_user(&mut self, message: MessageId) {
        if !self.users.contains(&message) {
            self.users.push_back(message);
        }
    }

    /// A wake signal arrived.
    pub fn note_wake(&mut self, now: Instant) {
        self.first_wake.get_or_insert(now);
        self.last_wake = Some(now);
    }

    /// At startup, or when a turn left wakes behind: the pending wakes are ready now.
    pub fn assume_wakes(&mut self, now: Instant) {
        self.note_wake(now);
        self.catch_up = true;
    }

    /// What runs next at `now`.
    pub fn next(&self, now: Instant) -> Next {
        if let Some(&message) = self.users.front() {
            return Next::User(message);
        }
        let (Some(first), Some(last)) = (self.first_wake, self.last_wake) else {
            return Next::Idle;
        };
        if self.catch_up {
            return Next::Wakes;
        }
        let ready_at = (last + WAKE_QUIET).min(first + WAKE_MAX_HOLD);
        if now >= ready_at {
            Next::Wakes
        } else {
            Next::WaitUntil(ready_at)
        }
    }

    /// Takes the next message of yours for its turn.
    pub fn take_user(&mut self) -> Option<MessageId> {
        self.users.pop_front()
    }

    /// A turn took the pending wakes: the wake window starts over.
    pub fn wakes_taken(&mut self) {
        self.first_wake = None;
        self.last_wake = None;
        self.catch_up = false;
    }

    /// Your messages waiting for a turn, oldest first: shown as queued.
    pub fn queued(&self) -> Vec<MessageId> {
        self.users.iter().copied().collect()
    }
}
