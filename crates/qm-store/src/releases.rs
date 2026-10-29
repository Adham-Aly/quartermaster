//! Release launches: one row per proposed release tag (`release_launches`).
//!
//! A launch binds your approval to the project, the repository (GitHub's numeric id and its name),
//! the tag and the commit. Its status only moves forward, and [`begin_push`] (the one move from
//! `proposed` to `pushing`) is the only gate in front of sending a tag: whoever wins it sends the
//! tag once, and nobody ever sends it again, after a retry or a restart alike.
//!
//! Once a launch is `published`, the app watches what GitHub shows for it (its release and the
//! workflow runs on its commit): [`start_observing`] marks it `observing`, [`save_observation`]
//! stores each reading as JSON (its shape belongs to `qm-core`), and the last one marks it `done`.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{ReleaseLaunchStatus, TagKind};
use qm_types::ids::space;
use qm_types::ids::{AgentId, CommitSha, HeldActionId, ProjectId, ReleaseLaunchId, TaskId};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// A launch to write, `proposed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewLaunch {
    pub task: TaskId,
    pub project: ProjectId,
    pub asked_by: Option<AgentId>,
    pub repo: String,
    pub repo_id: u64,
    pub tag: String,
    pub commit: CommitSha,
    pub branch: Option<String>,
    pub kind: TagKind,
    pub message: Option<String>,
    pub at: Timestamp,
}

/// A launch as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRow {
    pub id: ReleaseLaunchId,
    pub task: Option<TaskId>,
    pub project: Option<ProjectId>,
    pub held: Option<HeldActionId>,
    pub asked_by: Option<AgentId>,
    pub repo: String,
    pub repo_id: u64,
    pub tag: String,
    pub commit: CommitSha,
    pub branch: Option<String>,
    pub kind: TagKind,
    pub message: Option<String>,
    pub status: ReleaseLaunchStatus,
    /// The tag object the app wrote, for an annotated tag.
    pub tag_object: Option<String>,
    /// What the remote's tag pointed at when last read.
    pub remote_object: Option<String>,
    /// The last outcome, in plain words.
    pub remote_words: Option<String>,
    pub proposed_at: Timestamp,
    pub pushed_at: Option<Timestamp>,
    pub published_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    /// Whether the app is watching the published tag's release and workflow runs.
    pub observe_state: Option<ObserveState>,
    /// The last reading, as `qm-core` wrote it.
    pub observation: Option<serde_json::Value>,
    pub observed_at: Option<Timestamp>,
}

/// Whether a published launch's release and workflow runs are being watched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserveState {
    /// Read again until it settles or the watch runs out.
    Observing,
    /// The watch ended; only an explicit ask reads it again.
    Done,
}

impl ObserveState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observing => "observing",
            Self::Done => "done",
        }
    }

    fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "observing" => Ok(Self::Observing),
            "done" => Ok(Self::Done),
            other => Err(invalid("release_launches.observe_state", other)),
        }
    }
}

const COLUMNS: &str = "id, task_id, project_id, held_id, asked_by, repo, repo_id, tag, commit_sha, \
     branch, tag_kind, message, status, tag_object, remote_object, remote_words, proposed_at, \
     pushed_at, published_at, ended_at, observe_state, observation, observed_at";

fn invalid(column: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::Invalid {
        column,
        message: message.into(),
    }
}

fn millis(column: &'static str, ms: Option<i64>) -> Result<Option<Timestamp>, StoreError> {
    ms.map(|ms| from_millis(column, ms)).transpose()
}

type Raw = (
    (
        ReleaseLaunchId,
        Option<TaskId>,
        Option<ProjectId>,
        Option<HeldActionId>,
        Option<AgentId>,
    ),
    (String, i64, String, String, Option<String>, TagKind),
    (
        Option<String>,
        ReleaseLaunchStatus,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
    (i64, Option<i64>, Option<i64>, Option<i64>),
    (Option<String>, Option<String>, Option<i64>),
);

fn raw(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
        (
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
        ),
        (r.get(11)?, r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?),
        (r.get(16)?, r.get(17)?, r.get(18)?, r.get(19)?),
        (r.get(20)?, r.get(21)?, r.get(22)?),
    ))
}

fn row(raw: Raw) -> Result<LaunchRow, StoreError> {
    let (
        (id, task, project, held, asked_by),
        (repo, repo_id, tag, commit, branch, kind),
        rest,
        at,
        (observe_state, observation, observed),
    ) = raw;
    let (message, status, tag_object, remote_object, remote_words) = rest;
    let (proposed, pushed, published, ended) = at;
    Ok(LaunchRow {
        id,
        task,
        project,
        held,
        asked_by,
        repo,
        repo_id: u64::try_from(repo_id)
            .map_err(|_| invalid("release_launches.repo_id", repo_id.to_string()))?,
        tag,
        commit: commit
            .parse()
            .map_err(|e| invalid("release_launches.commit_sha", format!("{e}")))?,
        branch,
        kind,
        message,
        status,
        tag_object,
        remote_object,
        remote_words,
        proposed_at: from_millis("release_launches.proposed_at", proposed)?,
        pushed_at: millis("release_launches.pushed_at", pushed)?,
        published_at: millis("release_launches.published_at", published)?,
        ended_at: millis("release_launches.ended_at", ended)?,
        observe_state: observe_state
            .as_deref()
            .map(ObserveState::parse)
            .transpose()?,
        observation: observation
            .map(|o| {
                serde_json::from_str(&o)
                    .map_err(|e| invalid("release_launches.observation", e.to_string()))
            })
            .transpose()?,
        observed_at: millis("release_launches.observed_at", observed)?,
    })
}

fn query(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<LaunchRow>, StoreError> {
    let sql = format!("SELECT {COLUMNS} FROM release_launches WHERE {filter} ORDER BY seq");
    let raws = conn
        .prepare(&sql)?
        .query_map(args, raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(row).collect()
}

/// Writes a `proposed` launch. Fails on the live-tag unique index when the repository already has
/// a live launch of the same tag: look with [`live_for`] first.
pub fn insert_launch(conn: &Connection, new: &NewLaunch) -> Result<ReleaseLaunchId, StoreError> {
    let repo_id = i64::try_from(new.repo_id)
        .map_err(|_| invalid("release_launches.repo_id", new.repo_id.to_string()))?;
    let id = conn.query_row(
        "INSERT INTO release_launches (id, seq, task_id, project_id, asked_by, repo, repo_id, tag,
                                       commit_sha, branch, tag_kind, message, proposed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         RETURNING id",
        params![
            ids::fresh(conn, "release_launches", space::ROW)?,
            seq::next(new.at),
            new.task,
            new.project,
            new.asked_by,
            new.repo,
            repo_id,
            new.tag,
            new.commit.as_str(),
            new.branch,
            new.kind,
            new.message,
            to_millis(new.at),
        ],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Links a launch to its card.
pub fn set_held(
    conn: &Connection,
    id: ReleaseLaunchId,
    held: HeldActionId,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE release_launches SET held_id = ?2 WHERE id = ?1",
        params![id, held],
    )?;
    Ok(())
}

/// Reads a launch.
pub fn load(conn: &Connection, id: ReleaseLaunchId) -> Result<Option<LaunchRow>, StoreError> {
    Ok(query(conn, "id = ?1", params![id])?.into_iter().next())
}

/// The live launch of `tag` in the repository, if any (`proposed`, `pushing`, `unconfirmed` or
/// `published`).
pub fn live_for(
    conn: &Connection,
    repo_id: u64,
    tag: &str,
) -> Result<Option<LaunchRow>, StoreError> {
    let repo_id = i64::try_from(repo_id)
        .map_err(|_| invalid("release_launches.repo_id", repo_id.to_string()))?;
    Ok(query(
        conn,
        "repo_id = ?1 AND tag = ?2 \
         AND status IN ('proposed', 'pushing', 'unconfirmed', 'published')",
        params![repo_id, tag],
    )?
    .into_iter()
    .next())
}

/// The launch a card performs.
pub fn for_held(conn: &Connection, held: HeldActionId) -> Result<Option<LaunchRow>, StoreError> {
    Ok(query(conn, "held_id = ?1", params![held])?
        .into_iter()
        .next())
}

/// Launches whose push started and whose outcome the remote hasn't settled: `pushing` (the app
/// may have stopped mid-push) and `unconfirmed`.
pub fn unfinished(conn: &Connection) -> Result<Vec<LaunchRow>, StoreError> {
    query(conn, "status IN ('pushing', 'unconfirmed')", [])
}

/// The one gate in front of sending a tag: moves a `proposed` launch performed by card `held` to
/// `pushing`, recording the tag object about to be sent (an annotated tag's). True for the single
/// caller that moved it; false if it was already past `proposed` or belongs to another card, and
/// then nothing may be sent.
pub fn begin_push(
    conn: &Connection,
    id: ReleaseLaunchId,
    held: HeldActionId,
    tag_object: Option<&str>,
    at: Timestamp,
) -> Result<bool, StoreError> {
    let moved = conn.execute(
        "UPDATE release_launches SET status = 'pushing', pushed_at = ?3, tag_object = ?4
         WHERE id = ?1 AND held_id = ?2 AND status = 'proposed'",
        params![id, held, to_millis(at), tag_object],
    )?;
    Ok(moved == 1)
}

/// An outcome to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome<'a> {
    pub status: ReleaseLaunchStatus,
    /// What the remote's tag pointed at, if it was read.
    pub remote_object: Option<&'a str>,
    pub words: &'a str,
    pub at: Timestamp,
}

/// Moves a launch to `outcome.status`, only from one of `from`. True if it moved. A launch never
/// moves back to `proposed` or `pushing` here.
pub fn set_outcome(
    conn: &Connection,
    id: ReleaseLaunchId,
    from: &[ReleaseLaunchStatus],
    outcome: &Outcome<'_>,
) -> Result<bool, StoreError> {
    if matches!(
        outcome.status,
        ReleaseLaunchStatus::Proposed | ReleaseLaunchStatus::Pushing
    ) || from.is_empty()
    {
        return Ok(false);
    }
    let from = from
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let at = to_millis(outcome.at);
    let published = (outcome.status == ReleaseLaunchStatus::Published).then_some(at);
    let ended = outcome.status.is_final().then_some(at);
    let moved = conn.execute(
        &format!(
            "UPDATE release_launches
             SET status = ?2, remote_object = COALESCE(?3, remote_object), remote_words = ?4,
                 published_at = COALESCE(published_at, ?5), ended_at = COALESCE(ended_at, ?6)
             WHERE id = ?1 AND status IN ({from})"
        ),
        params![
            id,
            outcome.status,
            outcome.remote_object,
            outcome.words,
            published,
            ended
        ],
    )?;
    Ok(moved == 1)
}

/// Starts watching a `published` launch that was never watched. True if it started.
pub fn start_observing(conn: &Connection, id: ReleaseLaunchId) -> Result<bool, StoreError> {
    let moved = conn.execute(
        "UPDATE release_launches SET observe_state = 'observing'
         WHERE id = ?1 AND status = 'published' AND observe_state IS NULL",
        params![id],
    )?;
    Ok(moved == 1)
}

/// Stores a reading of a launch whose watch is in state `from`, leaving it in `to` (a watch never
/// goes from `done` back to `observing`). True if it was stored.
pub fn save_observation(
    conn: &Connection,
    id: ReleaseLaunchId,
    from: ObserveState,
    to: ObserveState,
    observation: &serde_json::Value,
    at: Timestamp,
) -> Result<bool, StoreError> {
    if from == ObserveState::Done && to == ObserveState::Observing {
        return Ok(false);
    }
    let moved = conn.execute(
        "UPDATE release_launches SET observe_state = ?3, observation = ?4, observed_at = ?5
         WHERE id = ?1 AND status = 'published' AND observe_state = ?2",
        params![
            id,
            from.as_str(),
            to.as_str(),
            observation.to_string(),
            to_millis(at)
        ],
    )?;
    Ok(moved == 1)
}

/// Published launches still to be watched: those `observing`, and those never watched (published
/// before the app watched releases, or the app stopped before it started).
pub fn to_observe(conn: &Connection) -> Result<Vec<LaunchRow>, StoreError> {
    query(
        conn,
        "status = 'published' AND (observe_state IS NULL OR observe_state = 'observing')",
        [],
    )
}

/// Every launch of a task, oldest first.
/// Every tag the app was asked to push for `project`, whatever became of it.
pub fn tags_of_project(conn: &Connection, project: ProjectId) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT tag FROM release_launches WHERE project_id = ?1")?;
    let rows = stmt.query_map([project], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn for_task(conn: &Connection, task: TaskId) -> Result<Vec<LaunchRow>, StoreError> {
    query(conn, "task_id = ?1", params![task])
}

/// The status of the launch a card performs, if it has one.
pub fn held_status(
    conn: &Connection,
    held: HeldActionId,
) -> Result<Option<ReleaseLaunchStatus>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT status FROM release_launches WHERE held_id = ?1",
            params![held],
            |r| r.get(0),
        )
        .optional()?)
}
