//! Directed dependency removal, through the real task service and durable wake inbox.
use super::*;
use crate::turns::forget_tests::Fixture;
use qm_store::tasks as store;
use qm_types::domain::{TaskOutcome, TaskStatus};
use rusqlite::{Connection, params};
use serde_json::json;

fn task(id: u32) -> TaskId {
    TaskId::new(id).unwrap()
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
fn refused(error: ToolFailure) -> String {
    let ToolFailure::Refused(r) = error else {
        panic!("expected refusal, got {error:?}")
    };
    r.reason
}
fn count(conn: &Connection, table: &str, predicate: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM {table} WHERE {predicate}");
    conn.query_row(&sql, [], |r| r.get(0)).unwrap()
}
fn row(conn: &Connection, id: u32) -> store::TaskRow {
    store::load(conn, task(id)).unwrap().unwrap()
}
struct UnlinkFixture {
    fixture: Fixture,
    svc: TaskService,
    scheduler: JoinHandle<()>,
}
impl UnlinkFixture {
    async fn new() -> Self {
        let fixture = Fixture::new().await;
        fixture.db().call(|conn| {
            conn.execute_batch("INSERT INTO secondmates (id, name, scope, created_at) VALUES (1, 'SM', 'p', 1);
                INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, created_at, seq)
                VALUES (6, 'secondmate', 1, 1, '/sm', 'idle', 1, 6);
                UPDATE tasks SET outcome = 'stopped' WHERE id = 1;")?;
            for id in 3..=5 {
                conn.execute("INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, branch, status, created_at, seq)
                    VALUES (?1, 1, 1, 'build', 'task', 'goal', 'direct_pr', ?2, 'running', 1, ?1)", params![id, format!("qm/task-{id}")])?;
                conn.execute("INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                    VALUES (?1, 'crewmate', 1, ?1, '/crew', 'idle', 1, ?1)", [id])?;
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
    fn host(&self) -> crate::tool_host::CoreToolHost {
        let deps = self.svc.deps();
        let conversation = Arc::new(crate::api::Conversation::new(
            deps.db.clone(),
            deps.clock.clone(),
            AgentId::new(1),
        ));
        crate::tool_host::CoreToolHost::new(deps.db.clone(), deps.clock.clone(), conversation)
            .with_tasks(self.svc.clone())
    }
}
impl Drop for UnlinkFixture {
    fn drop(&mut self) {
        self.scheduler.abort();
    }
}

#[tokio::test]
async fn unlink_checks_both_direct_reports_for_qm_and_secondmate_and_host_role() {
    let f = UnlinkFixture::new().await;
    let host = f.host();
    for (owner, role, other) in [(1, Role::Quartermaster, 6), (6, Role::Secondmate, 1)] {
        for (blocker_owner, blocked_owner) in [(other, owner), (owner, other), (owner, owner)] {
            f.read(move |conn| {
                conn.execute(
                    "UPDATE tasks SET supervisor_id = ?1 WHERE id = 1",
                    [blocker_owner],
                )?;
                conn.execute(
                    "UPDATE tasks SET supervisor_id = ?1 WHERE id = 3",
                    [blocked_owner],
                )?;
                store::insert_link(conn, task(1), task(3), Timestamp::now())?;
                Ok(())
            })
            .await;
            let result = host
                .unlink_tasks(&caller(owner, role), task(1), task(3))
                .await;
            if blocker_owner != owner || blocked_owner != owner {
                let foreign = task(if blocker_owner == owner { 3 } else { 1 });
                assert!(refused(result.unwrap_err()).contains(&foreign.to_string()));
                f.read(move |conn| {
                    assert_eq!(store::blockers_of(conn, task(3))?.len(), 1);
                    assert_eq!(count(conn, "wakes", "1"), i64::from(owner != 1));
                    Ok(())
                })
                .await;
            } else {
                let removed = result.unwrap(); // Even the closed, unsuccessful blocker is removable.
                assert_eq!((removed.blocker, removed.blocked), (task(1), task(3)));
            }
        }
    }
    // An otherwise owning caller cannot bypass the host's offered_to check with a worker role.
    let reason = refused(
        host.unlink_tasks(&caller(6, Role::Crewmate), task(1), task(3))
            .await
            .unwrap_err(),
    );
    assert!(
        reason.contains("unlink_tasks") && reason.contains("crewmate"),
        "{reason}"
    );
    f.read(|conn| {
        assert_eq!(count(conn, "events", "kind = 'task.unlinked'"), 2);
        assert_eq!(count(conn, "wakes", "kind = 'wait_over'"), 2);
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn unlink_requires_distinct_existing_endpoints_even_if_edge_is_absent() {
    let f = UnlinkFixture::new().await;
    let qm = caller(1, Role::Quartermaster);
    for (a, b, fragment) in [
        (3, 3, "itself"),
        (99, 3, "doesn't exist"),
        (3, 99, "doesn't exist"),
    ] {
        assert!(
            refused(f.svc.unlink_tasks(&qm, task(a), task(b)).await.unwrap_err())
                .contains(fragment)
        );
    }
    f.read(|conn| {
        assert_eq!(count(conn, "events", "kind = 'task.unlinked'"), 0);
        assert_eq!(count(conn, "wakes", "1"), 0);
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn unlink_only_removes_named_edge_and_wait_once_without_finishing_blocker() {
    let f = UnlinkFixture::new().await;
    let before = f
        .read(|conn| {
            for (a, b) in [(1, 3), (5, 3), (1, 4), (1, 2)] {
                store::insert_link(conn, task(a), task(b), Timestamp::now())?;
            }
            conn.execute(
                "UPDATE tasks SET status = 'waiting', waiting_on = ?1 WHERE id IN (3, 4)",
                [json!({"task": task(1)}).to_string()],
            )?;
            Ok((row(conn, 1), row(conn, 4), row(conn, 2)))
        })
        .await;
    let qm = caller(1, Role::Quartermaster);
    f.svc.unlink_tasks(&qm, task(1), task(3)).await.unwrap();
    f.svc.unlink_tasks(&qm, task(1), task(3)).await.unwrap();
    f.read(move |conn| {
        let blocked = row(conn, 3);
        assert_eq!(blocked.status, TaskStatus::Running);
        assert_eq!(blocked.waiting_on, None);
        assert_eq!(row(conn, 1), before.0);
        assert_eq!(row(conn, 4), before.1);
        assert_eq!(row(conn, 2), before.2);
        assert_eq!(
            store::blockers_of(conn, task(3))?
                .iter()
                .map(|b| b.task)
                .collect::<Vec<_>>(),
            [task(5)]
        );
        assert_eq!(
            store::blocked_by(conn, task(1))?
                .iter()
                .map(|b| b.0)
                .collect::<Vec<_>>(),
            [task(2), task(4)]
        );
        let events = store::history(conn, task(3))?;
        let removed: Vec<_> = events
            .iter()
            .filter(|e| e.kind == waits::UNLINKED_EVENT)
            .collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].agent, Some(AgentId::new(1)));
        assert_eq!(removed[0].payload["blocker"], json!(task(1)));
        assert_eq!(removed[0].payload["blocked"], json!(task(3)));
        assert_eq!(count(conn, "events", "kind = 'task.status'"), 1);
        assert_eq!(count(conn, "wakes", "1"), 1);
        let (target, kind, key, payload): (AgentId, String, String, String) = conn.query_row(
            "SELECT target_agent_id, kind, dedupe_key, payload FROM wakes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        assert_eq!(target, AgentId::new(3));
        assert_eq!(kind, "wait_over");
        assert_eq!(
            key,
            format!("unlinked:{}:{}:{}", task(3), task(1), removed[0].id)
        );
        assert!(payload.contains("other dependencies and task/decision waits still apply"));
        assert!(payload.contains("An external wait ends on your next turn as usual"));
        assert!(payload.contains("does not mean"));
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn unlink_preserves_other_waits_and_nonwaiting_statuses() {
    let f = UnlinkFixture::new().await;
    let qm = caller(1, Role::Quartermaster);
    // Even stale wait metadata on a non-Waiting task must not be rewritten.
    for (status, waiting, wake) in [
        ("waiting", json!({"task": task(5)}), true),
        ("waiting", json!({"external": "a deployment"}), true),
        ("waiting", json!({"decision": "which API?"}), true),
        ("queued", json!({"task": task(1)}), false),
        ("held", json!({"task": task(1)}), true),
        ("reported", json!({"task": task(1)}), true),
        ("settling", json!(null), false),
        ("landed", json!(null), false),
        ("closed", json!(null), false),
        ("stopped", json!(null), false),
        ("failed", json!(null), false),
        ("discarded", json!(null), false),
    ] {
        let before = f.read(move |conn| {
                conn.execute("DELETE FROM wakes", [])?;
                conn.execute(
                    "UPDATE tasks SET status = ?1, waiting_on = ?2, closed_at = CASE WHEN ?1 = 'closed' THEN 2 END, outcome = CASE WHEN ?1 = 'closed' THEN 'stopped' END WHERE id = 3",
                    params![
                        status,
                        if waiting.is_null() {
                            None
                        } else {
                            Some(waiting.to_string())
                        }
                    ],
                )?;
                store::insert_link(conn, task(1), task(3), Timestamp::now())?;
                Ok(row(conn, 3))
            }).await;
        f.svc.unlink_tasks(&qm, task(1), task(3)).await.unwrap();
        f.read(move |conn| {
            assert_eq!(row(conn, 3), before, "{status}");
            assert_eq!(
                store::blockers_of(conn, task(3))?,
                [] as [store::Blocker; 0]
            );
            assert_eq!(count(conn, "wakes", "1"), i64::from(wake), "{status}");
            assert_eq!(
                count(conn, "wakes", "target_agent_id = 3 AND kind = 'wait_over'"),
                i64::from(wake),
                "{status}"
            );
            Ok(())
        })
        .await;
    }
}

#[tokio::test]
async fn unlink_relink_wakes_again_but_repeated_calls_do_not() {
    let f = UnlinkFixture::new().await;
    let qm = caller(1, Role::Quartermaster);
    for _ in 0..2 {
        f.svc.link_tasks(&qm, task(5), task(3)).await.unwrap();
        f.svc.link_tasks(&qm, task(5), task(3)).await.unwrap();
        f.svc.unlink_tasks(&qm, task(5), task(3)).await.unwrap();
        f.svc.unlink_tasks(&qm, task(5), task(3)).await.unwrap();
    }
    f.read(|conn| {
        for (event, kind, prefix) in [
            (waits::LINKED_EVENT, "blocked_by", "blocked_by"),
            (waits::UNLINKED_EVENT, "wait_over", "unlinked"),
        ] {
            let events: Vec<_> = store::history(conn, task(3))?
                .into_iter()
                .filter(|e| e.kind == event)
                .collect();
            assert_eq!(events.len(), 2);
            let keys: Vec<String> = conn
                .prepare("SELECT dedupe_key FROM wakes WHERE kind = ?1 ORDER BY seq")?
                .query_map([kind], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert_eq!(
                keys,
                events
                    .iter()
                    .map(|e| format!("{prefix}:{}:{}:{}", task(3), task(5), e.id))
                    .collect::<Vec<_>>()
            );
            assert_ne!(keys[0], keys[1]);
        }
        assert_eq!(count(conn, "wakes", "1"), 4);
        Ok(())
    })
    .await;
}

#[derive(Debug)]
struct NoNetwork;
impl qm_github::TokenSource for NoNetwork {
    fn token(&self) -> BoxFuture<'_, Result<qm_github::GithubToken, qm_github::GithubError>> {
        Box::pin(async { panic!("folder settlement must not contact GitHub") })
    }
    fn invalidate(&self) {
        panic!("no GitHub login in this fixture")
    }
}
impl crate::delivery::ports::ParentPort for NoNetwork {
    fn task_settled(
        &self,
        _: &Connection,
        _: TaskId,
        _: &crate::settling::Fact,
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn unlink_unsuccessful_closed_blocker_allows_reported_code_to_settle_in_folder() {
    use crate::delivery::{DeliveryDeps, DeliveryService};
    use crate::settling::SettlingService;
    use crate::verification::{VerificationDeps, VerificationService};
    use qm_tools::defs::SettleOutcome;
    let f = UnlinkFixture::new().await;
    let deps = f.svc.deps();
    let folder = deps.home.to_string_lossy().into_owned();
    f.read(move |conn| {
        conn.execute("UPDATE projects SET path = ?1, delivery_mode = 'folder' WHERE id = 1", [folder])?;
        conn.execute_batch("UPDATE tasks SET status = 'reported', delivery_mode = 'folder', branch = NULL WHERE id = 3;
            INSERT INTO reports (id, task_id, agent_id, source, outcome, head_commit, headings, created_at, seq)
            VALUES (3, 3, 3, 'report_call', 'done', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '{\"verified\":\"local tests passed\"}', 1, 3);")?;
        store::insert_link(conn, task(1), task(3), Timestamp::now())?;
        Ok(())
    }).await;
    let verification = VerificationService::new(VerificationDeps {
        db: deps.db.clone(),
        clock: deps.clock.clone(),
        data_dir: deps.data_dir.clone(),
        health: deps.health.clone(),
        inbox: deps.inbox.clone(),
        chat: deps.chat.clone(),
    });
    let settling = SettlingService::new(DeliveryService::new(DeliveryDeps {
        tasks: f.svc.clone(),
        github: qm_github::GithubClient::new(Arc::new(NoNetwork)),
        gh: qm_github::fixed_gh(deps.home.join("no-gh")),
        verification: Arc::new(verification),
        parent: Arc::new(NoNetwork),
    }));
    let qm = caller(1, Role::Quartermaster);
    let reason = refused(settling.settle_task(&qm, task(3)).await.unwrap_err());
    assert!(
        reason.contains(&format!("{} waits on {}", task(3), task(1))),
        "{reason}"
    );
    assert!(
        reason.contains(&format!(
            "unlink_tasks with blocker={} and blocked={}",
            task(1),
            task(3)
        )),
        "{reason}"
    );
    assert!(reason.contains("retry settle_task"), "{reason}");
    f.svc.unlink_tasks(&qm, task(1), task(3)).await.unwrap();
    assert!(
        matches!(settling.settle_task(&qm, task(3)).await.unwrap(), SettleOutcome::Done { folder } if folder == deps.home.to_string_lossy())
    );
    f.read(|conn| {
        assert_eq!(row(conn, 3).status, TaskStatus::Landed);
        let blocker = row(conn, 1);
        assert_eq!(
            (blocker.status, blocker.outcome),
            (TaskStatus::Closed, Some(TaskOutcome::Stopped))
        );
        assert_eq!(count(conn, "held_actions", "1"), 0);
        assert_eq!(count(conn, "events", "kind = 'outcome.delivered'"), 1);
        Ok(())
    })
    .await;
}
