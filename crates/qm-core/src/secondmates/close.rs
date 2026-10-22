//! `close_secondmate`: retiring one of the Quartermaster's secondmates.
//!
//! The safer of the two behaviours: a secondmate with any open task is refused, never retired
//! with work still running, so nothing it supervises is dropped silently. Retiring marks its
//! `secondmates` and `agents` rows retired (nothing is deleted: its conversation, events and
//! routed requests stay) and stops its loop, which closes its session. Rows are
//! kept, so its ID stays taken and is not handed out again.

use super::{SecondmateService, event_kind, failed, refuse};
use qm_policy::scope::check_own_secondmate;
use qm_store::StoreError;
use qm_store::events::{self, NewEvent};
use qm_store::secondmates;
use qm_tools::defs::Ack;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::ids::{AgentId, SecondmateId, TurnId};
use rusqlite::Connection;

impl SecondmateService {
    /// `close_secondmate`: the caller's own secondmate only, and only once it has no open tasks
    /// and no running turn.
    pub async fn close(&self, caller: &Caller, id: SecondmateId) -> Result<Ack, ToolFailure> {
        if self.is_working(id) {
            return Err(refuse(format!(
                "{id} is in the middle of a turn; wait for it to finish, then call \
                 close_secondmate again"
            )));
        }
        let caller_agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.clock().now();
        self.db()
            .call(move |conn| close_in(conn, caller_agent, turn, id, at))
            .await
            .map_err(|e| failed(&e))??;
        self.stop_loop(id);
        // The folders go now, not at the next start: the loop is stopped and no turn runs.
        let data = self.inner.deps.data_dir.clone();
        let removed = tokio::task::spawn_blocking(move || remove_retired_folders(&data, id)).await;
        if let Ok(Err(e)) = removed {
            tracing::warn!(error = %e, "couldn't remove a retired secondmate's folders; the next start will");
        }
        Ok(Ack {
            note: format!(
                "{id} is retired: its session is closed and it has left the workers list. Its \
                 history is kept."
            ),
        })
    }
}

/// Removes a retired secondmate's folders: its home, its session and its plugin folder.
pub(crate) fn remove_retired_folders(
    data: &qm_types::data_dir::DataDir,
    id: SecondmateId,
) -> std::io::Result<()> {
    qm_types::data_dir::remove_path(&data.secondmate(id).root)
}

/// [`SecondmateService::close`]'s transaction.
pub(crate) fn close_in(
    conn: &mut Connection,
    caller: AgentId,
    turn: Option<TurnId>,
    id: SecondmateId,
    at: jiff::Timestamp,
) -> Result<Result<(), ToolFailure>, StoreError> {
    let tx = conn.transaction()?;
    let Some(record) = secondmates::load(&tx, id)? else {
        return Ok(Err(refuse(format!(
            "There's no active secondmate {id}; it may already be retired"
        ))));
    };
    if let Err(r) = check_own_secondmate(caller, id, record.parent) {
        return Ok(Err(r.into()));
    }
    let open = secondmates::open_tasks_of(&tx, record.agent)?;
    if !open.is_empty() {
        let list: Vec<String> = open
            .iter()
            .map(|(task, _, status)| format!("{task} ({status})"))
            .collect();
        return Ok(Err(refuse(format!(
            "{id} still has open tasks: {}. Route it a request to settle or stop them, or wait \
             until they are closed, then call close_secondmate again",
            list.join(", ")
        ))));
    }
    secondmates::retire(&tx, id, at)?;
    let summary = format!("{id} ({}) was retired", record.name);
    events::insert(
        &tx,
        &NewEvent {
            kind: event_kind::RETIRED,
            agent: Some(caller),
            task: None,
            turn,
            summary: &summary,
            payload: serde_json::json!({ "secondmate": id, "name": record.name }),
            at,
        },
    )?;
    tx.commit()?;
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::close_in;
    use qm_store::context::{WorkerRow, workers};
    use qm_store::schedules::{self, NewSchedule};
    use qm_store::secondmates::{self, NewSecondmate};
    use qm_store::wakes::{self, NewWake};
    use qm_tools::handler::ToolFailure;
    use qm_types::data_dir::DataDir;
    use qm_types::data_dir::DataDir;
    use qm_types::domain::WakeKind;
    use qm_types::ids::{AgentId, SecondmateId};
    use rusqlite::{Connection, params};
    use std::path::PathBuf;

    /// A migrated database in a throwaway folder, with the Quartermaster's row.
    struct Fixture {
        dir: PathBuf,
        conn: Connection,
        qm: AgentId,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("qm-core-close-sm-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch folder");
            let mut conn = qm_store::connection::open(&dir.join("state.db")).expect("open");
            qm_store::schema::migrate(&mut conn).expect("migrate");
            let qm = qm_store::seat::ensure_quartermaster(&conn, &dir, jiff::Timestamp::now())
                .expect("quartermaster")
                .id;
            Self { dir, conn, qm }
        }

        fn secondmate(&self, name: &str) -> (SecondmateId, AgentId) {
            let data_dir = DataDir::new(&self.dir);
            let r = secondmates::insert(
                &self.conn,
                &NewSecondmate {
                    name,
                    scope: "web",
                    projects: &[],
                    standing_branch: None,
                    parent: self.qm,
                    data_dir: &data_dir,
                    at: jiff::Timestamp::now(),
                },
            )
            .expect("insert");
            (r.id, r.agent)
        }

        fn open_task(&self, supervisor: AgentId) {
            self.conn
                .execute(
                    "INSERT INTO projects (name, path, origin, default_branch, delivery_mode,
                         merge_method, created_at, seq)
                     VALUES ('p', '/tmp/p', 'linked', 'main', 'direct_pr', 'merge', 0, ?1)",
                    params![qm_store::seq::next_millis(0)],
                )
                .expect("project");
            let project = self.conn.last_insert_rowid();
            self.conn
                .execute(
                    "INSERT INTO tasks (project_id, supervisor_id, kind, title, goal,
                         delivery_mode, branch, status, created_at, seq)
                     VALUES (?1, ?2, 'build', 't', 'g', 'direct_pr', 'qm/task-1', 'running', 0, ?3)",
                    params![project, supervisor, qm_store::seq::next_millis(1)],
                )
                .expect("task");
        }

        fn close(&mut self, caller: AgentId, id: SecondmateId) -> Result<(), ToolFailure> {
            close_in(&mut self.conn, caller, None, id, jiff::Timestamp::now()).expect("store")
        }

        fn listed(&self, id: SecondmateId) -> bool {
            workers(&self.conn)
                .expect("workers")
                .iter()
                .any(|w| matches!(w, WorkerRow::Secondmate(s) if s.id == id))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn refusal(r: Result<(), ToolFailure>) -> String {
        match r {
            Err(ToolFailure::Refused(r)) => r.to_string(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_idle_secondmate_is_retired_and_leaves_the_workers_list() {
        let mut f = Fixture::new("idle");
        let (id, agent) = f.secondmate("Web");
        assert!(f.listed(id));
        f.close(f.qm, id).expect("retired");
        assert!(
            !f.listed(id),
            "the worker is gone from the context block's source"
        );
        assert!(secondmates::load(&f.conn, id).expect("load").is_none());
        // History is kept: the rows and the event are still there.
        let kept: i64 = f
            .conn
            .query_row(
                "SELECT count(*) FROM agents WHERE id = ?1 AND status = 'retired'",
                params![agent],
                |r| r.get(0),
            )
            .expect("agent row");
        assert_eq!(kept, 1);
        let events: i64 = f
            .conn
            .query_row(
                "SELECT count(*) FROM events WHERE kind = 'secondmate.retired'",
                [],
                |r| r.get(0),
            )
            .expect("events");
        assert_eq!(events, 1);
        // A second close is refused, and the retired row keeps its number, so a new secondmate
        // with the same name gets another one.
        assert!(refusal(f.close(f.qm, id)).contains("no active secondmate"));
        let (next, _) = f.secondmate("Web");
        assert_ne!(next, id);
    }

    #[test]
    fn a_secondmate_with_open_tasks_is_refused() {
        let mut f = Fixture::new("open");
        let (id, agent) = f.secondmate("Web");
        f.open_task(agent);
        let why = refusal(f.close(f.qm, id));
        assert!(why.contains("still has open tasks"), "{why}");
        assert!(why.contains("T-0001"), "{why}");
        assert!(f.listed(id), "it stays until its tasks are closed");
    }

    #[test]
    fn only_its_parent_may_retire_it() {
        let mut f = Fixture::new("parent");
        let (id, _) = f.secondmate("Web");
        let (_, other) = f.secondmate("Api");
        assert!(refusal(f.close(other, id)).contains("isn't your secondmate"));
        assert!(f.listed(id));
    }

    /// Loads `state.db` the way a restart does: the kept rows, pruned, in a fresh connection.
    fn restart(f: &Fixture) -> Connection {
        let (conn, _mirror) =
            qm_store::mirror::Mirror::open(&f.dir.join("state.db")).expect("restart");
        conn
    }

    fn rows(conn: &Connection, sql: &str, id: SecondmateId) -> i64 {
        conn.query_row(sql, params![id], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn a_retired_secondmate_with_a_pending_schedule_survives_a_restart() {
        let mut f = Fixture::new("schedule");
        let (id, agent) = f.secondmate("Web");
        let now = jiff::Timestamp::now();
        schedules::insert(
            &f.conn,
            &NewSchedule {
                owner: agent,
                due_at: now,
                tz: "UTC",
                what: "check the build",
                turn: None,
                at: now,
            },
        )
        .expect("schedule");
        f.close(f.qm, id).expect("retired");
        assert_eq!(schedules::pending(&f.conn).expect("pending").len(), 0);
        // Retiring cancelled the schedule, so nothing keeps the agent: its parent goes with it.
        let conn = restart(&f);
        assert_eq!(
            rows(
                &conn,
                "SELECT COUNT(*) FROM agents WHERE secondmate_id = ?1",
                id
            ),
            0
        );
        assert_eq!(
            rows(&conn, "SELECT COUNT(*) FROM secondmates WHERE id = ?1", id),
            0
        );
    }

    #[test]
    fn a_retired_secondmate_still_wanted_by_a_wake_survives_a_restart() {
        let mut f = Fixture::new("wake");
        let (id, agent) = f.secondmate("Web");
        wakes::insert_wake(
            &f.conn,
            &NewWake {
                target: agent,
                kind: WakeKind::Checks,
                payload: serde_json::json!({}),
                dedupe_key: "checks-1".into(),
                task: None,
                at: jiff::Timestamp::now(),
            },
        )
        .expect("wake");
        f.close(f.qm, id).expect("retired");
        // The unconsumed wake keeps the agent row, so its parent row is kept with it, and the
        // restart links them the same way.
        let conn = restart(&f);
        assert_eq!(
            rows(
                &conn,
                "SELECT COUNT(*) FROM agents WHERE secondmate_id = ?1",
                id
            ),
            1
        );
        assert_eq!(
            rows(&conn, "SELECT COUNT(*) FROM secondmates WHERE id = ?1", id),
            1
        );
    }

    #[test]
    fn retiring_removes_the_secondmates_folders_at_once() {
        let base = std::env::temp_dir().join(format!("qm-retire-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = DataDir::new(base.join("home"));
        let id: SecondmateId = "SM-3".parse().expect("a secondmate");
        let home = data.secondmate(id).home;
        std::fs::create_dir_all(&home).expect("the home");
        std::fs::write(home.join("note.txt"), "x").expect("a file");
        let other: SecondmateId = "SM-4".parse().expect("a secondmate");
        std::fs::create_dir_all(data.secondmate(other).home).expect("another home");

        super::remove_retired_folders(&data, id).expect("removed");
        let gone = !data.secondmate(id).root.exists();
        let kept = data.secondmate(other).root.exists();
        let _ = std::fs::remove_dir_all(&base);
        assert!(gone, "a retired secondmate's folders go at retirement");
        assert!(kept, "another secondmate's folders stay");
    }
}
