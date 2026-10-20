//! Verification: levels, failed checks and repair counting, the Careful review, evidence.
//!
//! [`VerificationService`] holds what these need (the store, the clock, the task folders, the
//! logins' health for a hold's choices, and delivery's [`ci_port::CiPort`]); the rules themselves
//! are `qm_policy::{verification, repair, review, merge_gate}`, called unchanged.
//!
//! - [`levels`]: `set_verification` and the effective level everywhere.
//! - [`repair`]: `ci_diagnosis`, `ci_rerun`, and the push and result hooks delivery calls.
//! - [`gate`]: the `ReportGate` for done reports, and delivery's verification facts.
//! - [`review`]: the review's start, its verdicts, the re-review, the review brief's extras.
//! - [`evidence`]: the evidence folder's manifest.
//! - [`lines`]: the context block's task line.

pub mod ci_port;
pub mod evidence;
pub mod gate;
pub mod levels;
pub mod lines;
pub mod repair;
pub mod review;

use crate::inbox::{Inbox, Notify};
use crate::tasks::new_wake;
use crate::turn_io::ChatLog;
use ci_port::{CiPort, NoCi};
use jiff::Timestamp;
use qm_auth::LoginHealth;
use qm_store::StoreError;
use qm_store::agents;
use qm_store::db::Db;
use qm_types::clock::Clock;
use qm_types::data_dir::DataDir;
use qm_types::domain::{VerificationLevel, WakeKind};
use qm_types::ids::{AgentId, TaskId};
use rusqlite::Connection;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

/// What the verification service is built from.
pub struct VerificationDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub data_dir: DataDir,
    /// Which logins work: a hold's choice 2 offers only tiers with a usable model.
    pub health: Arc<LoginHealth>,
    /// Signals each wake's target after the commit.
    pub inbox: Inbox,
    /// The UI's change signal, so a fresh snapshot follows every change.
    pub chat: Arc<dyn ChatLog>,
}

impl fmt::Debug for VerificationDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerificationDeps").finish_non_exhaustive()
    }
}

struct Inner {
    deps: VerificationDeps,
    ci: RwLock<Arc<dyn CiPort>>,
}

/// Verification's facts and hooks. Cheap to clone.
#[derive(Clone)]
pub struct VerificationService {
    inner: Arc<Inner>,
}

impl fmt::Debug for VerificationService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerificationService")
            .field("ci", &self.ci())
            .finish_non_exhaustive()
    }
}

impl VerificationService {
    /// The service, with no delivery behind it yet ([`NoCi`]).
    pub fn new(deps: VerificationDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                ci: RwLock::new(Arc::new(NoCi)),
            }),
        }
    }

    /// Puts delivery's checks behind the service (delivery and verification each hold the other,
    /// so one of them is set after both exist).
    pub fn set_ci(&self, ci: Arc<dyn CiPort>) {
        *self
            .inner
            .ci
            .write()
            .unwrap_or_else(PoisonError::into_inner) = ci;
    }

    pub(crate) fn ci(&self) -> Arc<dyn CiPort> {
        Arc::clone(&self.inner.ci.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.deps.db
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        &*self.inner.deps.clock
    }

    pub(crate) fn data_dir(&self) -> &DataDir {
        &self.inner.deps.data_dir
    }

    pub(crate) fn health(&self) -> &LoginHealth {
        &self.inner.deps.health
    }

    /// After a commit: signals every wake target the transaction posted, and the UI.
    pub(crate) fn notify(&self, notify: &Notify) {
        self.inner.deps.inbox.notify(notify);
        self.inner.deps.chat.changed();
    }
}

/// A level as you see it: "Quick", "Default (standard)", "Careful (reviewed)".
pub fn level_name(level: VerificationLevel) -> &'static str {
    match level {
        VerificationLevel::Quick => "Quick",
        VerificationLevel::Standard => "Default (standard)",
        VerificationLevel::Reviewed => "Careful (reviewed)",
    }
}

/// A level's short name: "Quick", "Default", "Careful".
pub fn level_short(level: VerificationLevel) -> &'static str {
    match level {
        VerificationLevel::Quick => "Quick",
        VerificationLevel::Standard => "Default",
        VerificationLevel::Reviewed => "Careful",
    }
}

/// Posts a wake to `task`'s crewmate, in the caller's transaction. Returns its agent, or `None`
/// when the task has no crewmate (or the wake was already posted).
#[allow(clippy::too_many_arguments)]
pub(crate) fn wake_crewmate(
    conn: &Connection,
    notify: &mut Notify,
    task: TaskId,
    kind: WakeKind,
    key: String,
    text: &str,
    short: &str,
    at: Timestamp,
) -> Result<Option<AgentId>, StoreError> {
    let Some(agent) = agents::crewmate_of(conn, task)? else {
        return Ok(None);
    };
    let wake = new_wake(
        agent.id,
        kind,
        task,
        key,
        text,
        short,
        serde_json::json!({}),
        at,
    );
    Ok(Inbox::post(conn, &wake, notify)?.map(|_| agent.id))
}
