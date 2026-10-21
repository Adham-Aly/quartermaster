//! Fixtures for the tests that check a function against rows: an in-memory store with the schema
//! migrated and the quartermaster's agent in it, and helpers that add projects and tasks through
//! the store's own inserts. Nothing here touches the disk, except [`Scratch`], a folder that is
//! removed when dropped.

use jiff::Timestamp;
use qm_store::schema;
use qm_store::{projects, tasks};
use qm_types::domain::{DeliveryMode, MergeMethod, ProjectOrigin, TaskKind};
use qm_types::ids::{AgentId, ProjectId, TaskId};
use rusqlite::{Connection, params};
use serde_json::json;
use std::path::PathBuf;

/// A migrated in-memory database with foreign keys on, and the quartermaster's agent as agent 1.
pub(crate) fn store() -> Connection {
    let mut conn = Connection::open_in_memory().expect("an in-memory database");
    conn.pragma_update(None, "foreign_keys", true)
        .expect("foreign keys on");
    schema::migrate(&mut conn).expect("the schema");
    conn.execute(
        "INSERT INTO agents (role, home, status, created_at) VALUES ('quartermaster', '/qm', 'idle', 0)",
        [],
    )
    .expect("the quartermaster");
    conn
}

/// The quartermaster's agent, which every task in [`store`] is supervised by.
pub(crate) fn supervisor() -> AgentId {
    AgentId::new(1)
}

/// A project named `name` at `/work/<name>`, with the given origin and delivery mode.
pub(crate) fn project(
    conn: &Connection,
    name: &str,
    origin: ProjectOrigin,
    delivery: DeliveryMode,
) -> ProjectId {
    projects::insert(
        conn,
        &projects::NewProject {
            name: name.to_owned(),
            path: PathBuf::from(format!("/work/{name}")),
            origin,
            default_branch: "main".to_owned(),
            github_remote: None,
            delivery,
            merge_method: MergeMethod::Squash,
            at: Timestamp::UNIX_EPOCH,
        },
    )
    .expect("a project")
}

/// A queued task of `kind` in `project`, recorded in `delivery` mode.
pub(crate) fn task(
    conn: &Connection,
    project: ProjectId,
    kind: TaskKind,
    delivery: DeliveryMode,
) -> TaskId {
    tasks::insert(
        conn,
        &tasks::NewTask {
            project,
            supervisor: supervisor(),
            kind,
            title: "a task".to_owned(),
            goal: "a goal".to_owned(),
            user_words: String::new(),
            user_words_message: None,
            instructions: json!([]),
            constraints: json!([]),
            tests_required: false,
            builds_on: None,
            parent: None,
            retry_of: None,
            escalations: 0,
            later_steps: json!([]),
            secondmate_request: None,
            delivery,
            verification: None,
            base_branch: None,
            base_ref: None,
            base_commit: None,
            port_base: None,
            at: Timestamp::UNIX_EPOCH,
        },
    )
    .expect("a task")
}

/// Sets a task's status without any of the transitions the service would check.
pub(crate) fn set_status(conn: &Connection, task: TaskId, status: &str) {
    conn.execute(
        "UPDATE tasks SET status = ?2 WHERE id = ?1",
        params![task, status],
    )
    .expect("a status");
}

/// Closes a task the way the store records it: closed, with an outcome and a closing time.
pub(crate) fn close(conn: &Connection, task: TaskId) {
    conn.execute(
        "UPDATE tasks SET status = 'closed', outcome = 'discarded', closed_at = 0 WHERE id = ?1",
        params![task],
    )
    .expect("a closed task");
}

/// A folder under the system's temporary directory, named for its test, emptied at the start and
/// removed when dropped.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("qm-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
