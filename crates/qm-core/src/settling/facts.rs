//! A merge, a delivery or a landing becomes a fact: one transaction moves the task `settling →
//! landed`, records where the work went, writes the outcome events, wakes the supervisor with
//! `settled` and sends the fact up the parent channel.

use crate::delivery::ports::ParentPort;
use crate::inbox::{Inbox, Notify};
use crate::tasks::lifecycle::{TaskEvent, next_for};
use crate::tasks::new_wake;
use crate::tasks::waits::release_waiters;
use jiff::Timestamp;
use qm_store::StoreError;
use qm_store::delivery as delivery_store;
use qm_store::tasks as task_store;
use qm_types::domain::{TaskStatus, WakeKind};
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;

/// `events.kind` of a merged pull request.
pub const PR_MERGED_EVENT: &str = "pr.merged";
/// `events.kind` of a merged task, for `<recent_outcomes>`.
pub const OUTCOME_MERGED: &str = "outcome.merged";
/// `events.kind` of a delivered branch.
pub const OUTCOME_DELIVERED: &str = "outcome.delivered";
/// `events.kind` of work landed in your checkout.
pub const OUTCOME_LANDED: &str = "outcome.landed";

/// What landed, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    /// A pull request merged on GitHub at exactly `head` (read back after GitHub said so).
    MergedPr {
        pr: u64,
        head: CommitSha,
        merge_commit: CommitSha,
        base: String,
    },
    /// A branch pushed and its checks passed. Never merged by the app.
    Delivered { branch: String, head: CommitSha },
    /// The personal workspace: the branch is kept locally.
    DeliveredLocal { branch: String, head: CommitSha },
    /// A folder task: its changes are already in `folder`. There is no head or branch.
    DoneInFolder { folder: String },
    /// Merged into your checkout's branch.
    InPlace {
        branch: String,
        head: CommitSha,
        new_head: CommitSha,
        target: String,
    },
}

impl Fact {
    /// The head settled; `None` for a folder task.
    pub fn head(&self) -> Option<&CommitSha> {
        match self {
            Self::MergedPr { head, .. }
            | Self::Delivered { head, .. }
            | Self::DeliveredLocal { head, .. }
            | Self::InPlace { head, .. } => Some(head),
            Self::DoneInFolder { .. } => None,
        }
    }

    /// The commit the work is at now: the merge commit, the delivered head, the new checkout head.
    /// `None` for a folder task, which has no commit.
    fn landed_commit(&self) -> Option<&CommitSha> {
        match self {
            Self::MergedPr { merge_commit, .. } => Some(merge_commit),
            Self::Delivered { head, .. } | Self::DeliveredLocal { head, .. } => Some(head),
            Self::InPlace { new_head, .. } => Some(new_head),
            Self::DoneInFolder { .. } => None,
        }
    }

    /// Where it went: the base branch, the task branch, or your checkout's branch (a folder's
    /// path, for a folder task).
    fn landed_into(&self) -> &str {
        match self {
            Self::MergedPr { base, .. } => base,
            Self::Delivered { branch, .. } | Self::DeliveredLocal { branch, .. } => branch,
            Self::InPlace { target, .. } => target,
            Self::DoneInFolder { folder } => folder,
        }
    }

    fn as_json(&self) -> serde_json::Value {
        match self {
            Self::MergedPr {
                pr,
                head,
                merge_commit,
                base,
            } => serde_json::json!({
                "fact": "merged_pr", "pr": pr, "head": head, "merge_commit": merge_commit, "base": base,
            }),
            Self::Delivered { branch, head } => {
                serde_json::json!({ "fact": "delivered", "branch": branch, "head": head })
            }
            Self::DeliveredLocal { branch, head } => {
                serde_json::json!({ "fact": "delivered_local", "branch": branch, "head": head })
            }
            Self::DoneInFolder { folder } => {
                serde_json::json!({ "fact": "done_in_folder", "folder": folder })
            }
            Self::InPlace {
                branch,
                head,
                new_head,
                target,
            } => serde_json::json!({
                "fact": "in_place", "branch": branch, "head": head, "new_head": new_head, "target": target,
            }),
        }
    }
}

/// Records `fact` for `task` (in `settling`) in the caller's transaction. Returns `false` (and
/// writes nothing) when the task isn't settling any more.
pub fn landed(
    conn: &Connection,
    notify: &mut Notify,
    parent: &dyn ParentPort,
    task: TaskId,
    fact: &Fact,
    at: Timestamp,
) -> Result<bool, StoreError> {
    let Some(row) = task_store::load(conn, task)? else {
        return Ok(false);
    };
    if row.status != TaskStatus::Settling {
        return Ok(false);
    }
    let Ok(to) = next_for(task, row.status, TaskEvent::Landed) else {
        return Ok(false);
    };
    task_store::set_status(conn, task, row.status, to, at)?;
    delivery_store::set_landed(conn, task, fact.landed_commit(), fact.landed_into())?;
    let short = fact
        .head()
        .map(|h| h.short().to_owned())
        .unwrap_or_default();
    let payload = fact.as_json();
    let (outcome_kind, outcome, text) = match fact {
        Fact::MergedPr { pr, base, .. } => {
            task_store::event(
                conn,
                PR_MERGED_EVENT,
                task,
                None,
                &format!("PR #{pr} merged at {short} into {base}"),
                payload.clone(),
                at,
            )?;
            (
                OUTCOME_MERGED,
                format!("{task} merged #{pr}"),
                format!(
                    "{task} merged: PR #{pr} at {short} into {base}. Close it with close_task."
                ),
            )
        }
        Fact::Delivered { branch, .. } => (
            OUTCOME_DELIVERED,
            format!("{task} delivered {branch}"),
            format!(
                "{task} delivered: {branch} is pushed at {short} and its checks passed; the user merges it. Close it with close_task."
            ),
        ),
        Fact::DeliveredLocal { branch, .. } => (
            OUTCOME_DELIVERED,
            format!("{task} delivered {branch}, kept in the workspace"),
            format!(
                "{task} delivered: {branch} at {short} is kept in the workspace for the user to merge. Close it with close_task."
            ),
        ),
        Fact::DoneInFolder { folder } => (
            OUTCOME_DELIVERED,
            format!("{task} delivered in {folder}"),
            format!("{task} delivered: its changes are in {folder}. Close it with close_task."),
        ),
        Fact::InPlace { branch, target, .. } => (
            OUTCOME_LANDED,
            format!("{task} landed in {target}"),
            format!(
                "{task} landed: {branch} at {short} is merged into {target} in the user's checkout. Close it with close_task."
            ),
        ),
    };
    task_store::event(conn, outcome_kind, task, None, &outcome, payload, at)?;
    Inbox::post(
        conn,
        &new_wake(
            row.supervisor,
            WakeKind::Settled,
            task,
            format!("settled:{task}:{short}"),
            &text,
            &outcome,
            serde_json::json!({ "head": fact.head() }),
            at,
        ),
        notify,
    )?;
    parent.task_settled(conn, task, fact)?;
    release_waiters(conn, notify, task, TaskStatus::Landed, at)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_store as fx;
    use qm_types::domain::{DeliveryMode, ProjectOrigin, TaskKind};
    use rusqlite::params;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct CountingParent(AtomicUsize);

    impl ParentPort for CountingParent {
        fn task_settled(&self, _: &Connection, _: TaskId, _: &Fact) -> Result<(), StoreError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn folder_fact() -> Fact {
        Fact::DoneInFolder {
            folder: "/work/notes".to_owned(),
        }
    }

    #[test]
    fn a_folder_fact_has_no_commit_and_names_its_folder() {
        let fact = folder_fact();
        assert_eq!(fact.head(), None);
        assert_eq!(fact.landed_commit(), None);
        assert_eq!(fact.landed_into(), "/work/notes");
        assert_eq!(
            fact.as_json(),
            serde_json::json!({ "fact": "done_in_folder", "folder": "/work/notes" })
        );
    }

    #[test]
    fn landing_a_folder_task_records_its_folder_and_no_commit() {
        let conn = fx::store();
        let folder = fx::project(
            &conn,
            "personal",
            ProjectOrigin::Workspace,
            DeliveryMode::Folder,
        );
        let work = fx::task(&conn, folder, TaskKind::Build, DeliveryMode::Folder);
        fx::set_status(&conn, work, "settling");
        let parent = CountingParent::default();
        let mut notify = Notify::new();

        let recorded = landed(
            &conn,
            &mut notify,
            &parent,
            work,
            &folder_fact(),
            Timestamp::UNIX_EPOCH,
        )
        .expect("the fact is recorded");

        assert!(recorded);
        let row = task_store::load(&conn, work)
            .expect("the task")
            .expect("a row");
        assert_eq!(row.status, TaskStatus::Landed);
        let (commit, into): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT landed_commit, landed_into FROM tasks WHERE id = ?1",
                params![work],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the landing");
        assert_eq!((commit, into), (None, Some("/work/notes".to_owned())));
        let summary: String = conn
            .query_row(
                "SELECT summary FROM events WHERE task_id = ?1 AND kind = ?2",
                params![work, OUTCOME_DELIVERED],
                |r| r.get(0),
            )
            .expect("the outcome event");
        assert_eq!(summary, format!("{work} delivered in /work/notes"));
        assert_eq!(parent.0.load(Ordering::SeqCst), 1);
    }
}
