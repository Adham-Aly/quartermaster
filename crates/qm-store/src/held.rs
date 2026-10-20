//! Held actions: everything that waits for your yes is one kind of record.
//!
//! The tool call stores the exact [`HeldPayload`]; when you approve, the *app* performs exactly
//! that payload, and the agent never gets a second chance to change it. A merge's payload carries
//! the commit SHA, so if the branch has moved, the approval no longer applies
//! ([`MergePayload::head`] must still be the branch's head). A decision is a held task with fixed
//! choices the app writes ([`HeldChoice`]); the supervisor contributes only a short summary.
//!
//! The payload is stored as JSON in `held_actions.payload`, tagged by `kind` (the same wire names
//! as `qm_types::domain::HeldKind`, which the table's `CHECK` ties to the `kind` column).
//!
//! **Held kinds grow.** [`HeldPayload`] and `HeldKind` are `#[non_exhaustive]`, and
//! `held_actions.kind` has no closed `CHECK` (like `wakes.kind`; Rust validates the kind when a row
//! is read), so a new kind of held action is one variant in each enum and no migration.

use crate::StoreError;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::api::HeldAnswer;
pub use qm_types::api::HeldChoice;
use qm_types::domain::{GithubWriteAction, HeldKind, HeldStatus, MergeMethod, ProjectAction};
use qm_types::ids::{AgentId, CommitSha, HeldActionId, TaskId};
pub use qm_types::rules::HoldReason;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// The exact thing to perform on your yes. `#[non_exhaustive]`: a kind added later never breaks
/// another crate's `match`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum HeldPayload {
    Merge(MergePayload),
    GithubWrite(GithubWritePayload),
    AddProject(AddProjectPayload),
    Secondmate(SecondmatePayload),
    Decision(DecisionPayload),
    Push(PushPayload),
}

impl HeldPayload {
    pub fn kind(&self) -> HeldKind {
        match self {
            Self::Merge(_) => HeldKind::Merge,
            Self::GithubWrite(_) => HeldKind::GithubWrite,
            Self::AddProject(_) => HeldKind::AddProject,
            Self::Secondmate(_) => HeldKind::Secondmate,
            Self::Decision(_) => HeldKind::Decision,
            Self::Push(_) => HeldKind::Push,
        }
    }
}

/// A branch-only push, pinned to the approved commit and to what the app last pushed there. The app
/// never merges it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPayload {
    pub task: TaskId,
    /// The remote's name, `origin`.
    pub remote: String,
    /// The task branch on the remote, `qm/task-0044`.
    pub branch: String,
    /// The approved commit.
    pub head: CommitSha,
    /// The lease: what the app last pushed to that branch (`None`: it must not exist yet).
    #[serde(default)]
    pub expected_remote: Option<CommitSha>,
}

/// A merge, pinned to the approved commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "into", rename_all = "snake_case")]
pub enum MergePayload {
    /// Merge a pull request on GitHub at exactly `head`, with the asynchronous merge API and the
    /// SHA as a precondition.
    PullRequest {
        task: TaskId,
        /// `owner/name`.
        repo: String,
        pr: u64,
        head: CommitSha,
        base: String,
        method: MergeMethod,
    },
    /// Build in place: merge the task branch at exactly `head` into the branch your checkout has
    /// checked out, only if your working tree is clean.
    InPlace {
        task: TaskId,
        branch: String,
        head: CommitSha,
        checkout: String,
        target_branch: String,
    },
}

impl MergePayload {
    pub fn task(&self) -> TaskId {
        match self {
            Self::PullRequest { task, .. } | Self::InPlace { task, .. } => *task,
        }
    }

    /// The approved commit.
    pub fn head(&self) -> &CommitSha {
        match self {
            Self::PullRequest { head, .. } | Self::InPlace { head, .. } => head,
        }
    }
}

/// A GitHub write held because it is outside the crewmate's own project, or in an answer-only task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubWritePayload {
    pub task: TaskId,
    pub action: GithubWriteAction,
    /// `owner/name`, resolved by the app.
    pub repo: String,
    pub number: Option<u64>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

/// Link, clone or create a project, at a location you confirm or change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddProjectPayload {
    pub action: ProjectAction,
    pub name: String,
    pub path: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
}

/// A proposed secondmate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondmatePayload {
    pub name: String,
    pub scope: String,
    pub projects: Vec<String>,
    pub base_branch: Option<String>,
}

/// A task the app holds for your decision. The app writes the title, the reason and the choices;
/// `summary` is the supervisor's, from `present_hold`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionPayload {
    pub task: TaskId,
    pub reason: HoldReason,
    /// The app's one-line detail, e.g. "3 repair attempts used".
    pub detail: String,
    pub summary: Option<String>,
}

/// A held action to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewHeld {
    pub payload: HeldPayload,
    /// Empty for approvals (approve or reject); the fixed choices for a decision.
    pub choices: Vec<HeldChoice>,
    /// The card's title, e.g. the task's title.
    pub title: String,
    pub task: Option<TaskId>,
    pub asked_by: Option<AgentId>,
    pub at: Timestamp,
}

/// A held action as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRow {
    pub id: HeldActionId,
    pub kind: HeldKind,
    pub payload: HeldPayload,
    pub choices: Vec<HeldChoice>,
    pub title: String,
    pub task: Option<TaskId>,
    pub asked_by: Option<AgentId>,
    pub status: HeldStatus,
    pub answer: Option<HeldAnswer>,
    pub created_at: Timestamp,
    pub decided_at: Option<Timestamp>,
}

fn to_json<T: Serialize>(column: &'static str, v: &T) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json { column, source })
}

fn from_json<T: for<'de> Deserialize<'de>>(column: &'static str, s: &str) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(|source| StoreError::Json { column, source })
}

/// Writes a pending held action.
pub fn insert_held(conn: &Connection, held: &NewHeld) -> Result<HeldActionId, StoreError> {
    let id = conn.query_row(
        "INSERT INTO held_actions (kind, payload, choices, title, task_id, asked_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
        params![
            held.payload.kind(),
            to_json("held_actions.payload", &held.payload)?,
            to_json("held_actions.choices", &held.choices)?,
            held.title,
            held.task,
            held.asked_by,
            to_millis(held.at),
        ],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Reads a held action back.
pub fn load_held(conn: &Connection, id: HeldActionId) -> Result<HeldRow, StoreError> {
    let (kind, payload, choices, title, task, asked_by, status, answer, created, decided) = conn
        .query_row(
            "SELECT kind, payload, choices, title, task_id, asked_by, status, answer, created_at, decided_at
             FROM held_actions WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, HeldKind>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<TaskId>>(4)?,
                    r.get::<_, Option<AgentId>>(5)?,
                    r.get::<_, HeldStatus>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                ))
            },
        )?;
    Ok(HeldRow {
        id,
        kind,
        payload: from_json("held_actions.payload", &payload)?,
        choices: from_json("held_actions.choices", &choices)?,
        title,
        task,
        asked_by,
        status,
        answer: answer
            .map(|a| from_json("held_actions.answer", &a))
            .transpose()?,
        created_at: from_millis("held_actions.created_at", created)?,
        decided_at: decided
            .map(|d| from_millis("held_actions.decided_at", d))
            .transpose()?,
    })
}
