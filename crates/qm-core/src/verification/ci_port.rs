//! What verification needs from delivery.
//!
//! The delivery service implements [`CiPort`] (its stored `checks_state` and its rerun); until
//! it is wired, [`NoCi`] answers that no checks are known and that reruns aren't available, which
//! is safe: a direct-PR done report then can't pass its checks gate, and nothing is rerun.

use qm_policy::merge_gate::ChecksVerdict;
use qm_store::StoreError;
use qm_tools::handler::ToolFailure;
use qm_types::BoxFuture;
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;
use std::fmt;

/// The pushed head's checks as delivery last stored them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiFacts {
    /// The commit these checks ran on.
    pub head: CommitSha,
    /// No check is pending any more: the verdict is final for this head.
    pub final_verdict: bool,
    pub verdict: ChecksVerdict,
    /// The failing checks' names.
    pub failing: Vec<String>,
    pub passed: u32,
    pub total: u32,
    /// The commit the app last pushed for the task (`tasks.pushed_head`).
    pub pushed_head: Option<CommitSha>,
}

/// What verification needs from delivery.
pub trait CiPort: Send + Sync + fmt::Debug {
    /// The pushed head's checks as last stored (no network).
    fn latest(&self, conn: &Connection, task: TaskId) -> Result<Option<CiFacts>, StoreError>;

    /// Reruns one failing check on the pushed head and re-arms the poller. A commit status from
    /// an outside service is refused ("upload-test is a commit status from an outside service; it
    /// can't be rerun from here").
    fn rerun<'a>(&'a self, task: TaskId, check: &'a str) -> BoxFuture<'a, Result<(), ToolFailure>>;

    /// Delivery's per-task lock, shared by `pr_open` and settling: a level change that takes a task
    /// out of `settling` holds it too. `None` without delivery.
    fn locks(&self) -> Option<crate::delivery::TaskLocks> {
        None
    }
}

/// No delivery wired: no checks known, no reruns.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCi;

impl CiPort for NoCi {
    fn latest(&self, conn: &Connection, task: TaskId) -> Result<Option<CiFacts>, StoreError> {
        let _ = (conn, task);
        Ok(None)
    }

    fn rerun<'a>(&'a self, task: TaskId, check: &'a str) -> BoxFuture<'a, Result<(), ToolFailure>> {
        let _ = (task, check);
        Box::pin(async { Err(ToolFailure::not_available("ci_rerun")) })
    }
}

/// Delivery's stored checks and its rerun: `checks_state` as the CI poller last wrote it, and
/// `DeliveryService::rerun_check`.
impl CiPort for crate::delivery::DeliveryService {
    fn latest(&self, conn: &Connection, task: TaskId) -> Result<Option<CiFacts>, StoreError> {
        let Some(row) = qm_store::delivery::load(conn, task)? else {
            return Ok(None);
        };
        let Some(state) = row.checks else {
            return Ok(None);
        };
        Ok(Some(CiFacts {
            verdict: crate::delivery::checks::stored_verdict(&state),
            head: state.head,
            final_verdict: state.is_final,
            failing: state.failing,
            passed: state.passed,
            total: state.total,
            pushed_head: row.pushed_head,
        }))
    }

    fn rerun<'a>(&'a self, task: TaskId, check: &'a str) -> BoxFuture<'a, Result<(), ToolFailure>> {
        Box::pin(self.rerun_check(task, check))
    }

    fn locks(&self) -> Option<crate::delivery::TaskLocks> {
        Some(crate::delivery::DeliveryService::locks(self).clone())
    }
}
