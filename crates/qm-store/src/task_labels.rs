//! Task labels: the `T-` labels that group crewmates (`C-`) working towards one thing.
//!
//! A label is a number, when it was made, and the name the crew list shows for it, if it was given
//! one. Whether it is open, and what it holds, comes from
//! the `tasks` rows that name it: `label_id` says which task a crewmate belongs to now, and
//! `folder_label_id` which task's folders hold its artifacts and scratchpad (they differ only while
//! a relabel waits to move them). A label no row names is deleted when closed crewmates are
//! forgotten, and its number reserved so it isn't reused while the app runs.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::to_millis;
use jiff::Timestamp;
use qm_types::ids::{TaskId, TaskLabelId, space};
use rusqlite::{Connection, params};

/// Makes a label, numbered `preferred` when that number is free: a crewmate's first task takes the
/// crewmate's own number, so `T-0042` holds `C-0042`. Call inside the caller's transaction.
pub fn create(
    conn: &Connection,
    preferred: Option<u32>,
    name: Option<&str>,
    at: Timestamp,
) -> Result<TaskLabelId, StoreError> {
    let number = match preferred {
        Some(n) => ids::fresh_preferring(conn, "task_labels", space::TASK_LABEL, n)?,
        None => ids::fresh(conn, "task_labels", space::TASK_LABEL)?,
    };
    let id = TaskLabelId::new(number).ok_or_else(|| StoreError::Invalid {
        column: "task_labels.id",
        message: format!("{number} is out of range for a task ID"),
    })?;
    conn.execute(
        "INSERT INTO task_labels (id, created_at, seq, name) VALUES (?1, ?2, ?3, ?4)",
        params![id, to_millis(at), seq::next(at), name],
    )?;
    Ok(id)
}

/// Names `label`, unless it already has a name: a task keeps the name it started with.
pub fn name_if_unnamed(
    conn: &Connection,
    label: TaskLabelId,
    name: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE task_labels SET name = ?2 WHERE id = ?1 AND name IS NULL",
        params![label, name],
    )?;
    Ok(())
}

/// `label`'s name, if it was given one.
pub fn name(conn: &Connection, label: TaskLabelId) -> Result<Option<String>, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT name FROM task_labels WHERE id = ?1",
            params![label],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// Whether `label` exists.
pub fn exists(conn: &Connection, label: TaskLabelId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM task_labels WHERE id = ?1)",
        params![label],
        |r| r.get(0),
    )?)
}

/// The crewmates `label` holds, oldest first, with whether each is still open.
pub fn crewmates(conn: &Connection, label: TaskLabelId) -> Result<Vec<(TaskId, bool)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, status <> 'closed' FROM tasks
         WHERE COALESCE(label_id, id) = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![label], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Deletes every label no crewmate names, returning their numbers (the caller reserves them once
/// its transaction commits).
pub fn forget_unused(conn: &Connection) -> Result<Vec<u32>, StoreError> {
    let mut stmt = conn.prepare(
        "DELETE FROM task_labels
         WHERE id NOT IN (SELECT COALESCE(label_id, id) FROM tasks)
           AND id NOT IN (SELECT COALESCE(folder_label_id, id) FROM tasks)
         RETURNING id",
    )?;
    let numbers = stmt
        .query_map([], |r| r.get::<_, u32>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(numbers)
}
