//! Holds and decision cards.
//!
//! A hold is a held action of kind `decision`: the app writes the title, the reason and the
//! choices ([`hold`], [`cards`]); the supervisor adds one short summary with `present_hold`
//! ([`present`]); when you pick, the app carries out your choice exactly ([`performer`]). The
//! approval card's Diff button opens the in-app Diff screen ([`diff`]).

pub mod cards;
pub mod diff;
pub mod hold;
pub mod performer;
pub mod present;

use crate::inbox::{Inbox, Notify};
use crate::tasks::TaskService;
use crate::turn_io::ChatLog;
use qm_store::db::Db;
use qm_types::clock::Clock;
use std::fmt;
use std::sync::Arc;

/// What the decision service is built from.
pub struct DecisionDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub inbox: Inbox,
    /// The UI's change signal.
    pub chat: Arc<dyn ChatLog>,
    /// The side exits, the retry and your words to a crewmate.
    pub tasks: TaskService,
    /// Reads a held merge's diff for the Diff screen.
    pub diff: Arc<dyn diff::DiffReader>,
}

impl fmt::Debug for DecisionDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionDeps").finish_non_exhaustive()
    }
}

/// `present_hold`, carrying out your choice, and the Diff screen. Cheap to clone.
#[derive(Clone)]
pub struct DecisionService {
    inner: Arc<DecisionDeps>,
}

impl fmt::Debug for DecisionService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionService").finish_non_exhaustive()
    }
}

impl DecisionService {
    pub fn new(deps: DecisionDeps) -> Self {
        Self {
            inner: Arc::new(deps),
        }
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.db
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        &*self.inner.clock
    }

    pub(crate) fn tasks(&self) -> &TaskService {
        &self.inner.tasks
    }

    pub(crate) fn diff_reader(&self) -> &Arc<dyn diff::DiffReader> {
        &self.inner.diff
    }

    /// After a commit: signals every wake target the transaction posted, and the UI.
    pub(crate) fn notify(&self, notify: &Notify) {
        self.inner.inbox.notify(notify);
        self.inner.chat.changed();
    }

    /// The UI's change signal alone.
    pub(crate) fn changed(&self) {
        self.inner.chat.changed();
    }
}
