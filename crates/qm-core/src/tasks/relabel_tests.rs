//! Moving crewmates between tasks, through the real task service, store and data directory.
use super::*;
use crate::turns::forget_tests::Fixture;
use qm_store::tasks as store;
use rusqlite::{Connection, params};

fn crewmate(id: u32) -> TaskId {
    TaskId::new(id).unwrap()
}
fn label(id: u32) -> TaskLabelId {
    TaskLabelId::new(id).unwrap()
}
fn caller(agent: i64, role: Role) -> Caller {
    Caller {
        agent: AgentId::new(agent),
        role,
        task: None,
        secondmate: None,
        turn: None,
        plan: None,
    }
}
fn qm() -> Caller {
    caller(1, Role::Quartermaster)
}
fn refused(error: ToolFailure) -> String {
    let ToolFailure::Refused(r) = error else {
        panic!("expected refusal, got {error:?}")
    };
    r.reason
}

/// Crewmates C-3, C-4 and C-5 run under the Quartermaster, each in a task of its own numbered
/// like it; C-7 runs under secondmate S-1 (agent 6).
struct RelabelFixture {
    fixture: Fixture,
    svc: TaskService,
    scheduler: JoinHandle<()>,
}
impl RelabelFixture {
    async fn new() -> Self {
        let fixture = Fixture::new().await;
        fixture.db().call(|conn| {
            conn.execute_batch("INSERT INTO secondmates (id, name, scope, created_at) VALUES (1, 'SM', 'p', 1);
                INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, created_at, seq)
                VALUES (6, 'secondmate', 1, 1, '/sm', 'idle', 1, 6);")?;
            for (id, supervisor) in [(3, 1), (4, 1), (5, 1), (7, 6)] {
                conn.execute(
                    "INSERT INTO task_labels (id, created_at, seq) VALUES (?1, 1, ?1)",
                    [id],
                )?;
                conn.execute("INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, branch, status, created_at, seq, label_id, folder_label_id)
                    VALUES (?1, 1, ?2, 'build', 'task', 'goal', 'direct_pr', ?3, 'running', 1, ?1, ?1, ?1)", params![id, supervisor, format!("qm/task-{id}")])?;
                conn.execute("INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                    VALUES (?1, 'crewmate', ?2, ?1, '/crew', 'idle', 1, ?1)", params![id, supervisor])?;
            }
            qm_store::seq::observe_all(conn)?;
            Ok(())
        }).await.unwrap();
        let (svc, scheduler) = fixture.task_service();
        Self {
            fixture,
            svc,
            scheduler,
        }
    }
    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> T {
        self.fixture.db().call(f).await.unwrap()
    }
    async fn row(&self, id: u32) -> store::TaskRow {
        self.read(move |conn| store::load(conn, crewmate(id)))
            .await
            .unwrap()
    }
    async fn wakes(&self, agent: i64) -> Vec<(String, String)> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, payload ->> 'text' FROM wakes WHERE target_agent_id = ?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([agent], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }
    fn paths(&self, id: u32, folders: u32) -> TaskPaths {
        self.svc.deps().data_dir.task(crewmate(id), label(folders))
    }
}
impl Drop for RelabelFixture {
    fn drop(&mut self) {
        self.scheduler.abort();
    }
}

#[tokio::test]
async fn joining_a_task_moves_the_folders_and_tells_the_crewmate() {
    let f = RelabelFixture::new().await;
    let old = f.paths(3, 3);
    std::fs::create_dir_all(&old.evidence).unwrap();
    std::fs::write(old.evidence.join("before.txt"), "seen").unwrap();
    std::fs::create_dir_all(&old.scratchpad).unwrap();

    let changed = f
        .svc
        .set_task_label(&qm(), crewmate(3), Some(label(4)))
        .await
        .unwrap();
    assert_eq!(
        (changed.crewmate, changed.task, changed.was, changed.with),
        (crewmate(3), label(4), label(3), vec![crewmate(4)])
    );

    // No runner: the folders follow at once, and the old paths still lead there.
    let row = f.row(3).await;
    assert_eq!((row.label, row.folder_label), (label(4), label(4)));
    let new = f.paths(3, 4);
    assert_eq!(
        std::fs::read_to_string(new.evidence.join("before.txt")).unwrap(),
        "seen"
    );
    assert!(new.scratchpad.is_dir());
    for link in [&old.artifacts, &old.scratchpad] {
        assert!(std::fs::symlink_metadata(link).unwrap().is_symlink());
    }
    assert_eq!(
        std::fs::read_to_string(old.evidence.join("before.txt")).unwrap(),
        "seen"
    );

    let wakes = f.wakes(3).await;
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert_eq!(wakes[0].0, "task_changed");
    assert!(
        wakes[0].1.contains("task T-0004, with C-0004"),
        "{}",
        wakes[0].1
    );
    assert!(wakes[0].1.contains(&new.evidence.display().to_string()));

    // T-0003 holds nobody now; moving back replaces the link left behind with the folder.
    let back = f
        .svc
        .set_task_label(&qm(), crewmate(3), Some(label(3)))
        .await;
    assert!(refused(back.unwrap_err()).contains("T-0003 doesn't exist"));
    let own = f
        .svc
        .set_task_label(&qm(), crewmate(3), None)
        .await
        .unwrap();
    assert_ne!(own.task, label(4));
    assert_eq!(own.with, Vec::<TaskId>::new());
    let moved = f.paths(3, own.task.get());
    assert!(
        !std::fs::symlink_metadata(&moved.artifacts)
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(moved.evidence.join("before.txt")).unwrap(),
        "seen"
    );
    assert_eq!(f.wakes(3).await.len(), 2);
}

#[tokio::test]
async fn a_crewmate_alone_in_its_task_stays_there_when_given_a_task_of_its_own() {
    let f = RelabelFixture::new().await;
    let changed = f
        .svc
        .set_task_label(&qm(), crewmate(5), None)
        .await
        .unwrap();
    assert_eq!((changed.task, changed.was), (label(5), label(5)));
    assert_eq!(f.wakes(5).await, Vec::<(String, String)>::new());
    let events: i64 = f
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM events WHERE kind = 'crewmate.task_changed'",
                [],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(events, 0);
}

#[tokio::test]
async fn moving_is_refused_for_closed_crewmates_finished_tasks_and_others_crew() {
    let f = RelabelFixture::new().await;
    f.read(|conn| {
        conn.execute(
            "UPDATE tasks SET status = 'closed', outcome = 'landed', closed_at = 2 WHERE id = 5",
            [],
        )?;
        Ok(())
    })
    .await;
    let closed = f
        .svc
        .set_task_label(&qm(), crewmate(5), Some(label(4)))
        .await;
    assert!(refused(closed.unwrap_err()).contains("C-0005 is closed"));
    let finished = f
        .svc
        .set_task_label(&qm(), crewmate(3), Some(label(5)))
        .await;
    assert!(refused(finished.unwrap_err()).contains("T-0005 is finished"));
    let missing = f
        .svc
        .set_task_label(&qm(), crewmate(3), Some(label(99)))
        .await;
    assert!(refused(missing.unwrap_err()).contains("T-0099 doesn't exist"));

    // A secondmate moves only its own crewmates, and only into its own tasks.
    let sm = caller(6, Role::Secondmate);
    assert!(f.svc.set_task_label(&sm, crewmate(3), None).await.is_err());
    let foreign = f.svc.set_task_label(&sm, crewmate(7), Some(label(4))).await;
    assert!(refused(foreign.unwrap_err()).contains("holds none of your crewmates"));
    assert_eq!(f.row(7).await.label, label(7));
}

#[tokio::test]
async fn a_task_given_for_a_crewmate_names_its_one_crewmate() {
    let f = RelabelFixture::new().await;
    // Its own task: T-0003 is C-0003.
    assert_eq!(
        f.svc.resolve_task_label(label(3)).await.unwrap(),
        crewmate(3)
    );
    // Two open crewmates: ambiguous.
    f.svc
        .set_task_label(&qm(), crewmate(3), Some(label(4)))
        .await
        .unwrap();
    let both = refused(f.svc.resolve_task_label(label(4)).await.unwrap_err());
    assert!(both.contains("C-0003, C-0004"), "{both}");
    // One closed: the open one is meant.
    f.read(|conn| {
        conn.execute(
            "UPDATE tasks SET status = 'closed', outcome = 'landed', closed_at = 2 WHERE id = 4",
            [],
        )?;
        Ok(())
    })
    .await;
    assert_eq!(
        f.svc.resolve_task_label(label(4)).await.unwrap(),
        crewmate(3)
    );
    // An emptied task that still exists names nobody.
    let empty = refused(f.svc.resolve_task_label(label(3)).await.unwrap_err());
    assert!(empty.contains("T-0003 holds no crewmates"), "{empty}");
    // A label that never existed, numbered like a crewmate from before tasks held crewmates.
    f.read(|conn| {
        conn.execute("DELETE FROM task_labels WHERE id = 3", [])?;
        Ok(())
    })
    .await;
    assert_eq!(
        f.svc.resolve_task_label(label(3)).await.unwrap(),
        crewmate(3)
    );
}

#[tokio::test]
async fn find_tasks_lists_a_tasks_crewmates_and_each_crewmates_task() {
    let f = RelabelFixture::new().await;
    f.svc
        .set_task_label(&qm(), crewmate(3), Some(label(4)))
        .await
        .unwrap();
    let caller = qm();
    let find = |key: &str| {
        let query: FindTasksArgs =
            serde_json::from_value(serde_json::json!({ "key": key })).unwrap();
        f.svc.find_tasks(&caller, query)
    };
    let found = find("T-0004").await.unwrap();
    let mut held: Vec<(TaskId, TaskLabelId)> =
        found.tasks.iter().map(|t| (t.task, t.label)).collect();
    held.sort();
    assert_eq!(held, vec![(crewmate(3), label(4)), (crewmate(4), label(4))]);
    // Each result names the crewmate and its task.
    let json = serde_json::to_value(&found.tasks[0]).unwrap();
    assert_eq!(json["task"], "T-0004");
    assert!(json["crewmate"].as_str().unwrap().starts_with("C-"));
    // A task nobody is in reads as the crewmate numbered like it.
    let old = find("T-0003").await.unwrap();
    assert_eq!(old.tasks.len(), 1);
    assert_eq!(
        (old.tasks[0].task, old.tasks[0].label),
        (crewmate(3), label(4))
    );
    // A crewmate is itself.
    assert_eq!(find("C-0004").await.unwrap().tasks.len(), 1);
}

#[tokio::test]
async fn the_task_pane_shows_each_crewmates_task_and_who_shares_it() {
    let f = RelabelFixture::new().await;
    f.svc
        .set_task_label(&qm(), crewmate(3), Some(label(4)))
        .await
        .unwrap();
    let view = crate::crew_view::load(f.fixture.db(), &qm_types::clock::SystemClock, None)
        .await
        .unwrap();
    let task_row = |id: u32| -> String {
        let pane = view.tasks.iter().find(|d| d.task == crewmate(id)).unwrap();
        let row = pane
            .rows
            .iter()
            .find(|r| r.label.as_deref() == Some("task"))
            .unwrap();
        row.value.iter().map(|s| s.text.as_str()).collect()
    };
    assert_eq!(task_row(3), "Task · with 1 collaborator");
    assert_eq!(task_row(4), "Task · with 1 collaborator");
    assert_eq!(task_row(5), "Task");
    // Friendly display strings do not replace the typed selection/grouping IDs.
    for (id, expected_label) in [(3, label(4)), (4, label(4)), (5, label(5))] {
        let agent = AgentRef::Crewmate { task: crewmate(id) };
        let row = view.crew.iter().find(|row| row.agent == agent).unwrap();
        assert_eq!(row.task.as_ref().unwrap().id, expected_label);
        assert!(view.tasks.iter().any(|pane| pane.task == crewmate(id)));
    }
}
