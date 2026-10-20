//! The facts snapshot of one request, and which events make an update. Pure:
//! no store, no clock.
//!
//! ```json
//! { "state": "working",
//!   "tasks": [ { "id": "T-0045", "kind": "build", "state": "ci_passed", "pr": 231 } ],
//!   "outcomes": [ { "at": "14:31", "type": "pr_ready", "task": "T-0045", "pr": 231, "sha": "9c0d2e4" } ],
//!   "source_event": 8812 }
//! ```

use crate::tasks::lifecycle::is_open;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_store::delivery::verdict;
use qm_store::parent_channel::{FactEvent, SnapshotInputs, SnapshotTask};
use qm_types::domain::{TaskKind, TaskStatus};
use qm_types::ids::{EventId, TaskId};
use serde::Serialize;

/// The event kinds the pump reads, as the merged code writes them (one table, so renaming a kind
/// is a one-line change). Each maps to an outcome in [`outcome_of`]; `task.status` maps only for
/// the statuses that are facts.
///
/// | Event (writer) | Outcome |
/// |---|---|
/// | `task.started` | `started` |
/// | `task.reported` (`tasks/report.rs`, payload `outcome`) | `reported` (`state`: done / blocked / needs_decision) |
/// | `task.status` → `held` / `stuck` / `landed` / `failed` (`qm_store::tasks::set_status`, payload `to`) | `held` / `stuck` / `landed` / `failed` |
/// | `task.closed` (`tasks/close.rs`, payload `outcome`) | `closed` (`outcome`) |
/// | `pr.opened` (`delivery/pr.rs`, payload `pr`, `head`) | `pr_ready` |
/// | `checks.passed` · `checks.failed` (`delivery/checks.rs`, payload `head`) | `ci_passed` · `ci_failed` |
/// | `pr.merged` (`settling/facts.rs`, payload `pr`, `merge_commit`) | `merged` |
pub const FACT_KINDS: &[&str] = &[
    TASK_STARTED,
    TASK_REPORTED,
    TASK_STATUS,
    TASK_CLOSED,
    PR_OPENED,
    CHECKS_PASSED,
    CHECKS_FAILED,
    PR_MERGED,
];

const TASK_STARTED: &str = crate::tasks::start::STARTED_EVENT;
const TASK_REPORTED: &str = crate::tasks::report::REPORTED_EVENT;
const TASK_STATUS: &str = qm_store::tasks::STATUS_EVENT;
const TASK_CLOSED: &str = crate::tasks::close::CLOSED_EVENT;
const PR_OPENED: &str = crate::delivery::pr::PR_OPENED_EVENT;
const CHECKS_PASSED: &str = crate::delivery::checks::CHECKS_PASSED_EVENT;
const CHECKS_FAILED: &str = crate::delivery::checks::CHECKS_FAILED_EVENT;
const PR_MERGED: &str = crate::settling::facts::PR_MERGED_EVENT;

/// The most outcomes a snapshot keeps (the newest).
pub const MAX_OUTCOMES: usize = 20;

/// What kind of fact an outcome is (`outcomes[].type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeType {
    Started,
    Reported,
    Held,
    Stuck,
    Landed,
    Closed,
    Failed,
    PrReady,
    CiPassed,
    CiFailed,
    Merged,
}

impl OutcomeType {
    /// The word as stored and shown (`pr_ready`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Reported => "reported",
            Self::Held => "held",
            Self::Stuck => "stuck",
            Self::Landed => "landed",
            Self::Closed => "closed",
            Self::Failed => "failed",
            Self::PrReady => "pr_ready",
            Self::CiPassed => "ci_passed",
            Self::CiFailed => "ci_failed",
            Self::Merged => "merged",
        }
    }
}

/// One fact on a request's task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub at: Timestamp,
    pub kind: OutcomeType,
    pub task: TaskId,
    pub task_kind: TaskKind,
    pub pr: Option<i64>,
    /// The commit, short (seven characters).
    pub sha: Option<String>,
    /// A report's state (`done`, `blocked`, `needs_decision`) or a close's outcome word.
    pub detail: Option<String>,
    /// The event it came from.
    pub event: EventId,
}

/// The snapshot, serialised into `parent_channel.facts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Facts {
    /// `working` or `idle`.
    pub state: &'static str,
    pub tasks: Vec<TaskFacts>,
    pub outcomes: Vec<OutcomeFacts>,
    pub source_event: Option<EventId>,
}

/// One task in the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskFacts {
    pub id: TaskId,
    pub kind: TaskKind,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<i64>,
}

/// One outcome in the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutcomeFacts {
    /// Local `HH:MM`.
    pub at: String,
    #[serde(rename = "type")]
    pub kind: OutcomeType,
    pub task: TaskId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// A report's state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// A close's outcome word.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

fn text(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn short_sha(payload: &serde_json::Value, key: &str) -> Option<String> {
    text(payload, key).map(|s| s.chars().take(7).collect())
}

/// The outcome an event stands for, or `None` when it makes no update (every other status
/// change, so `queued → preparing → running` wakes nobody).
pub fn outcome_of(event: &FactEvent) -> Option<Outcome> {
    let p = &event.payload;
    let pr = p.get("pr").and_then(serde_json::Value::as_i64);
    let (kind, sha, detail) = match event.kind.as_str() {
        TASK_STARTED => (OutcomeType::Started, None, None),
        TASK_REPORTED => (OutcomeType::Reported, None, text(p, "outcome")),
        TASK_CLOSED => (OutcomeType::Closed, None, text(p, "outcome")),
        TASK_STATUS => {
            let kind = match text(p, "to")?.parse::<TaskStatus>().ok()? {
                TaskStatus::Held => OutcomeType::Held,
                TaskStatus::Stuck => OutcomeType::Stuck,
                TaskStatus::Landed => OutcomeType::Landed,
                TaskStatus::Failed => OutcomeType::Failed,
                _ => return None,
            };
            (kind, None, None)
        }
        PR_OPENED => (OutcomeType::PrReady, short_sha(p, "head"), None),
        CHECKS_PASSED => (OutcomeType::CiPassed, short_sha(p, "head"), None),
        CHECKS_FAILED => (OutcomeType::CiFailed, short_sha(p, "head"), None),
        PR_MERGED => (
            OutcomeType::Merged,
            short_sha(p, "merge_commit").or_else(|| short_sha(p, "head")),
            None,
        ),
        _ => return None,
    };
    Some(Outcome {
        at: event.at,
        kind,
        task: event.task,
        task_kind: event.task_kind,
        pr,
        sha,
        detail,
        event: event.id,
    })
}

/// A task's state word: its status, or the delivery word while its pull request is in play
/// (`pr_open`, `ci_passed`, `ci_failed`).
pub fn task_state(task: &SnapshotTask) -> String {
    let in_delivery = matches!(
        task.status,
        TaskStatus::Running
            | TaskStatus::Waiting
            | TaskStatus::Reported
            | TaskStatus::InReview
            | TaskStatus::Settling
    );
    if in_delivery && task.pr.is_some() {
        return match task.checks.as_deref() {
            Some(verdict::PASSED) => "ci_passed",
            Some(verdict::FAILED | verdict::MISSING_REQUIRED) => "ci_failed",
            _ => "pr_open",
        }
        .to_owned();
    }
    task.status.to_string()
}

/// `working` when the secondmate is in a turn or any of the request's tasks is open.
pub fn request_state(inputs: &SnapshotInputs) -> &'static str {
    if inputs.secondmate_running || inputs.tasks.iter().any(|t| is_open(t.status)) {
        "working"
    } else {
        "idle"
    }
}

/// Local `HH:MM` in `tz`.
pub fn local_hhmm(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone()).strftime("%H:%M").to_string()
}

/// The request's outcomes, oldest first, the newest [`MAX_OUTCOMES`].
pub fn outcomes(inputs: &SnapshotInputs) -> Vec<Outcome> {
    let mut all: Vec<Outcome> = inputs.events.iter().filter_map(outcome_of).collect();
    let extra = all.len().saturating_sub(MAX_OUTCOMES);
    all.drain(..extra);
    all
}

/// The facts snapshot for one request.
pub fn snapshot(inputs: &SnapshotInputs, tz: &TimeZone) -> Facts {
    Facts {
        state: request_state(inputs),
        tasks: inputs
            .tasks
            .iter()
            .map(|t| TaskFacts {
                id: t.id,
                kind: t.kind,
                state: task_state(t),
                pr: t.pr,
            })
            .collect(),
        outcomes: outcomes(inputs)
            .into_iter()
            .map(|o| {
                let (state, outcome) = match o.kind {
                    OutcomeType::Reported => (o.detail, None),
                    OutcomeType::Closed => (None, o.detail),
                    _ => (None, None),
                };
                OutcomeFacts {
                    at: local_hhmm(o.at, tz),
                    kind: o.kind,
                    task: o.task,
                    pr: o.pr,
                    sha: o.sha,
                    state,
                    outcome,
                }
            })
            .collect(),
        source_event: inputs.source_event,
    }
}
