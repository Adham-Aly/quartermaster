//! The durable wake inbox: writing wakes with their facts and signalling their targets.
//!
//! **The rule.** A wake is inserted with [`Inbox::post`] **inside the transaction that records its
//! fact** (a report row, a job's exit, a status change), and its target is signalled with
//! [`Inbox::notify`] only after that transaction commits. A signal carries no data: the `wakes`
//! table is the truth. A lost signal (a crash right after the commit) is caught up when the
//! consumer next starts: [`Inbox::register`] signals once, and every consumer claims what is
//! pending when it starts.
//!
//! Consumers register a [`WakeSink`]: the seat a [`SeatSink`], each crewmate's runner its
//! `CrewHandle`, each secondmate's loop its own. Delivery is at-least-once: a turn claims its wakes
//! when it starts and consumes them in the transaction that finishes it, and the dedupe key keeps
//! the same news from being written twice.

use crate::turn_io::{SeatInbox, SeatInput};
use qm_store::StoreError;
use qm_store::wakes::{self, NewWake};
use qm_types::ids::{AgentId, WakeId};
use rusqlite::Connection;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

/// Something that wants to know a wake for its agent was committed.
pub trait WakeSink: Send + Sync + fmt::Debug {
    /// Called after the commit; must not block.
    fn wake(&self);
}

/// The agents to signal once a transaction that posted wakes commits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notify(BTreeSet<AgentId>);

impl Notify {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a target by hand (a wake posted another way).
    pub fn add(&mut self, agent: AgentId) {
        self.0.insert(agent);
    }

    /// Takes in another set's targets.
    pub fn extend(&mut self, other: Notify) {
        self.0.extend(other.0);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The agents to signal.
    pub fn targets(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.0.iter().copied()
    }
}

/// Who listens for wakes, by agent.
#[derive(Clone, Default)]
pub struct Inbox {
    sinks: Arc<RwLock<HashMap<AgentId, Arc<dyn WakeSink>>>>,
}

impl fmt::Debug for Inbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self
            .sinks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        f.debug_struct("Inbox").field("sinks", &n).finish()
    }
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the consumer for `agent`, replacing any earlier one, then signals it once so it
    /// catches up with wakes committed while nobody listened.
    pub fn register(&self, agent: AgentId, sink: &Arc<dyn WakeSink>) {
        self.sinks
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(agent, Arc::clone(sink));
        sink.wake();
    }

    /// Forgets the consumer for `agent`. Wakes for it stay in the table.
    pub fn unregister(&self, agent: AgentId) {
        self.sinks
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&agent);
    }

    /// Writes a wake inside the caller's transaction, so the wake and its fact commit together,
    /// and remembers its target for [`Inbox::notify`]. `None` when the dedupe key was already
    /// used for that target: the same news never wakes anyone twice.
    pub fn post(
        conn: &Connection,
        wake: &NewWake,
        notify: &mut Notify,
    ) -> Result<Option<WakeId>, StoreError> {
        let id = wakes::insert_wake(conn, wake)?;
        if id.is_some() {
            notify.add(wake.target);
        }
        Ok(id)
    }

    /// After the commit: signals each target's consumer. An unregistered target is fine: its
    /// consumer claims the wake when it starts.
    pub fn notify(&self, notify: &Notify) {
        let sinks: Vec<Arc<dyn WakeSink>> = {
            let map = self.sinks.read().unwrap_or_else(PoisonError::into_inner);
            notify
                .targets()
                .filter_map(|agent| map.get(&agent).cloned())
                .collect()
        };
        for sink in sinks {
            sink.wake();
        }
    }
}

/// The seat's consumer: a wake for the Quartermaster reaches its turn loop as `SeatInput::Wake`.
#[derive(Clone, Debug)]
pub struct SeatSink(pub SeatInbox);

impl WakeSink for SeatSink {
    fn wake(&self) {
        if self.0.send(SeatInput::Wake).is_err() {
            tracing::debug!("a wake for the Quartermaster arrived after its loop stopped");
        }
    }
}
