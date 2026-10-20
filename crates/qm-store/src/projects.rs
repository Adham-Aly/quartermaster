//! The project registry's rows.
//!
//! A project is a local git repository with settings. Rows are never deleted: removing a project
//! sets `removed_at` and leaves its folder alone, so tasks and events that name it still resolve
//! ([`load`] returns removed projects too). Names are matched case-insensitively among the
//! projects not removed ([`active_by_name`]); the table's own unique index is a backstop.

use crate::StoreError;
use crate::time::to_millis;
use jiff::Timestamp;
use qm_types::domain::{
    Autonomy, DeliveryMode, MergeMethod, ProjectOrigin, TaskStatus, VerificationLevel,
};
use qm_types::ids::{ProjectId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

/// A project as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub path: PathBuf,
    pub origin: ProjectOrigin,
    pub default_branch: String,
    /// `owner/name` when the project's `origin` is on GitHub.
    pub github_remote: Option<String>,
    pub delivery: DeliveryMode,
    pub autonomy: Autonomy,
    pub verification: VerificationLevel,
    pub merge_method: MergeMethod,
    /// Files copied from your checkout into each new worktree, relative paths.
    pub files_to_copy: Vec<PathBuf>,
    pub setup_command: Option<String>,
    pub removed: bool,
}

/// A project to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewProject {
    pub name: String,
    pub path: PathBuf,
    pub origin: ProjectOrigin,
    pub default_branch: String,
    pub github_remote: Option<String>,
    pub delivery: DeliveryMode,
    pub merge_method: MergeMethod,
    pub at: Timestamp,
}

/// The settings `configure_project` may change (never autonomy or the verification level). `None`
/// leaves a setting as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsChange {
    pub delivery: Option<DeliveryMode>,
    pub merge_method: Option<MergeMethod>,
    pub default_branch: Option<String>,
    pub files_to_copy: Option<Vec<PathBuf>>,
    /// `Some(None)` clears the setup command.
    pub setup_command: Option<Option<String>>,
}

const COLUMNS: &str = "id, name, path, origin, default_branch, github_remote, delivery_mode, \
                       autonomy, verification_level, merge_method, files_to_copy, setup_command, \
                       removed_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ProjectRow, String)> {
    Ok((
        ProjectRow {
            id: r.get(0)?,
            name: r.get(1)?,
            path: PathBuf::from(r.get::<_, String>(2)?),
            origin: r.get(3)?,
            default_branch: r.get(4)?,
            github_remote: r.get(5)?,
            delivery: r.get(6)?,
            autonomy: r.get(7)?,
            verification: r.get(8)?,
            merge_method: r.get(9)?,
            files_to_copy: Vec::new(),
            setup_command: r.get(11)?,
            removed: r.get::<_, Option<i64>>(12)?.is_some(),
        },
        r.get(10)?,
    ))
}

fn finish((mut project, files): (ProjectRow, String)) -> Result<ProjectRow, StoreError> {
    let files: Vec<String> = serde_json::from_str(&files).map_err(|source| StoreError::Json {
        column: "projects.files_to_copy",
        source,
    })?;
    project.files_to_copy = files.into_iter().map(PathBuf::from).collect();
    Ok(project)
}

fn query(
    conn: &Connection,
    filter: &str,
    values: impl rusqlite::Params,
) -> Result<Vec<ProjectRow>, StoreError> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM projects {filter}"))?;
    stmt.query_map(values, row)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(finish)
        .collect()
}

fn path_text(path: &std::path::Path) -> Result<&str, StoreError> {
    path.to_str().ok_or_else(|| StoreError::Invalid {
        column: "projects.path",
        message: format!("{} isn't valid UTF-8", path.display()),
    })
}

/// The project not removed whose name matches `name`, ignoring case.
pub fn active_by_name(conn: &Connection, name: &str) -> Result<Option<ProjectRow>, StoreError> {
    Ok(query(
        conn,
        "WHERE removed_at IS NULL AND name = ?1 COLLATE NOCASE ORDER BY id LIMIT 1",
        params![name],
    )?
    .pop())
}

/// The project not removed registered at exactly `path`.
pub fn active_by_path(
    conn: &Connection,
    path: &std::path::Path,
) -> Result<Option<ProjectRow>, StoreError> {
    Ok(query(
        conn,
        "WHERE removed_at IS NULL AND path = ?1 ORDER BY id LIMIT 1",
        params![path_text(path)?],
    )?
    .pop())
}

/// A project by ID, removed ones too.
pub fn load(conn: &Connection, id: ProjectId) -> Result<Option<ProjectRow>, StoreError> {
    Ok(query(conn, "WHERE id = ?1", params![id])?.pop())
}

/// Every project not removed: registered ones in the order they were added, the built-in
/// workspace last.
pub fn active(conn: &Connection) -> Result<Vec<ProjectRow>, StoreError> {
    query(
        conn,
        "WHERE removed_at IS NULL ORDER BY origin = 'workspace', id",
        [],
    )
}

/// The built-in personal workspace, if it has been set up.
pub fn workspace(conn: &Connection) -> Result<Option<ProjectRow>, StoreError> {
    Ok(query(conn, "WHERE origin = 'workspace' ORDER BY id LIMIT 1", [])?.pop())
}

/// The folder of the most recently linked project on this Mac, removed ones included: the
/// suggested location for a new project is its parent.
pub fn latest_linked_path(conn: &Connection) -> Result<Option<PathBuf>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT path FROM projects WHERE origin = 'linked'
             ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from))
}

/// Registers a project.
pub fn insert(conn: &Connection, project: &NewProject) -> Result<ProjectId, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO projects (name, path, origin, default_branch, github_remote, delivery_mode,
                               merge_method, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
        params![
            project.name,
            path_text(&project.path)?,
            project.origin,
            project.default_branch,
            project.github_remote,
            project.delivery,
            project.merge_method,
            to_millis(project.at),
        ],
        |r| r.get(0),
    )?)
}

/// Applies a settings change to a project not removed. Returns whether a row changed.
pub fn change_settings(
    conn: &Connection,
    id: ProjectId,
    change: &SettingsChange,
) -> Result<bool, StoreError> {
    let files = change
        .files_to_copy
        .as_ref()
        .map(|files| {
            let texts = files
                .iter()
                .map(|f| path_text(f).map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()?;
            serde_json::to_string(&texts).map_err(|source| StoreError::Json {
                column: "projects.files_to_copy",
                source,
            })
        })
        .transpose()?;
    let (set_setup, setup) = match &change.setup_command {
        None => (false, None),
        Some(command) => (true, command.clone()),
    };
    let changed = conn.execute(
        "UPDATE projects SET
             delivery_mode  = coalesce(?2, delivery_mode),
             merge_method   = coalesce(?3, merge_method),
             default_branch = coalesce(?4, default_branch),
             files_to_copy  = coalesce(?5, files_to_copy),
             setup_command  = CASE WHEN ?6 THEN ?7 ELSE setup_command END
         WHERE id = ?1 AND removed_at IS NULL",
        params![
            id,
            change.delivery,
            change.merge_method,
            change.default_branch,
            files,
            set_setup,
            setup,
        ],
    )?;
    Ok(changed == 1)
}

/// Unregisters a project; its folder is never touched. Returns whether a row changed.
pub fn mark_removed(conn: &Connection, id: ProjectId, at: Timestamp) -> Result<bool, StoreError> {
    Ok(conn.execute(
        "UPDATE projects SET removed_at = ?2 WHERE id = ?1 AND removed_at IS NULL",
        params![id, to_millis(at)],
    )? == 1)
}

/// The project's tasks that aren't closed, oldest first.
pub fn open_tasks(conn: &Connection, id: ProjectId) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT id FROM tasks WHERE project_id = ?1 AND status <> 'closed' ORDER BY id")?;
    Ok(stmt
        .query_map(params![id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The statuses in which a task counts as work running in its project (the filled dot in the
/// projects pane).
pub const WORKING: &[TaskStatus] = &[
    TaskStatus::Preparing,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Stuck,
    TaskStatus::InReview,
    TaskStatus::Settling,
];

/// The projects with work running in them.
pub fn with_work_running(conn: &Connection) -> Result<Vec<ProjectId>, StoreError> {
    let statuses = WORKING
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT project_id FROM tasks WHERE status IN ({statuses}) ORDER BY project_id"
    ))?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
