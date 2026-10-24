//! Report tool → `TaskService` → SQLite/wake → UI fill, without an external agent or browser.
use super::*;
use crate::turns::forget_tests::Fixture;
use qm_tools::defs::{Ack, ReportArgs};
use qm_tools::handler::{Caller, Dispatcher, ToolCx, ToolFailure, ToolHost};
use qm_types::domain::{Role, TaskKind};
use qm_types::ids::AgentId;
use serde_json::{Value, json};
use std::fs;

/// Only the host adapter is test-specific; dispatch, validation and persistence are real.
#[derive(Debug)]
struct ReportHost(TaskService);
impl ToolHost for ReportHost {
    fn record_report<'a>(
        &'a self,
        caller: &'a Caller,
        report: ReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(self.0.record_report(caller, report))
    }
}

struct Delivery {
    fixture: Fixture,
    host: ReportHost,
    scheduler: tokio::task::JoinHandle<()>,
    caller: Caller,
}
impl Delivery {
    async fn new(kind: TaskKind) -> Self {
        let fixture = Fixture::new().await;
        fixture.db().call(move |conn| {
            conn.execute(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, status, created_at, seq)
                 VALUES (3, 1, 1, ?1, 'Findings', 'investigate', 'folder', 'running', 1, 3)",
                [kind.as_str()],
            )?;
            conn.execute_batch(
                "INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                 VALUES (3, 'crewmate', 1, 3, '/c3', 'idle', 1, 3);",
            )?;
            qm_store::seq::observe_all(conn)?;
            Ok(())
        }).await.unwrap();
        let (svc, scheduler) = fixture.task_service();
        let caller = Caller {
            agent: AgentId::new(3),
            role: Role::Crewmate,
            task: Some(TaskId::new(3).unwrap()),
            secondmate: None,
            turn: None,
            plan: None,
        };
        // Seed exactly as start does, but don't launch an engine in this fixture.
        fs::create_dir_all(
            svc.deps()
                .data_dir
                .task(caller.task.unwrap(), caller.task.unwrap().own_label())
                .artifacts,
        )
        .unwrap();
        crate::reports::prepare(
            &svc.deps()
                .data_dir
                .task(caller.task.unwrap(), caller.task.unwrap().own_label()),
            kind,
        )
        .unwrap();
        Self {
            fixture,
            host: ReportHost(svc),
            scheduler,
            caller,
        }
    }

    fn page(&self) -> PathBuf {
        self.host
            .0
            .deps()
            .data_dir
            .task(
                self.caller.task.unwrap(),
                self.caller.task.unwrap().own_label(),
            )
            .artifacts
            .join("report/report.html")
    }

    fn write_page(&self) {
        fs::write(self.page(), "<!doctype html><html><head><title>Findings</title></head><body><h1>Result</h1><p>Checked locally.</p></body></html>").unwrap();
    }

    async fn report(&self, args: Value) -> qm_engine::ToolResult {
        Dispatcher::builtin()
            .dispatch(
                &ToolCx {
                    caller: &self.caller,
                    clock: self.host.0.clock(),
                    host: &self.host,
                },
                "report",
                args,
            )
            .await
            .result
    }

    async fn fill(&self) -> Snapshot {
        let mut snapshot = Snapshot::default();
        ReportPart::new(self.host.0.clone())
            .fill(&mut snapshot)
            .await;
        snapshot
    }

    async fn assert_delivered(&self, kind: TaskKind, answer: &'static str) {
        self.fixture
            .db()
            .call(move |conn| {
                let row = tasks::load(conn, TaskId::new(3).unwrap())?.unwrap();
                assert_eq!(row.status, qm_types::domain::TaskStatus::Reported);
                assert!(row.reported_at.is_some());
                let reports = qm_store::reports::for_task(conn, row.id)?;
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].answer.as_deref(), Some(answer));
                assert_eq!(
                    reports[0].reproduced,
                    if kind == TaskKind::Reproduce {
                        Some(true)
                    } else {
                        None
                    }
                );
                let (wake_kind, text): (String, String) = conn.query_row(
                    "SELECT kind, json_extract(payload, '$.text') FROM wakes WHERE target_agent_id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                assert_eq!(wake_kind, "reported");
                assert!(text.contains(answer), "{text}");
                if kind == TaskKind::Reproduce {
                    assert!(text.contains("Reproduced: yes."));
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn assert_unreported(&self) {
        self.fixture
            .db()
            .call(|conn| {
                assert_eq!(
                    conn.query_row("SELECT status FROM tasks WHERE id = 3", [], |r| r
                        .get::<_, String>(0))?,
                    "running"
                );
                assert!(
                    tasks::load(conn, TaskId::new(3).unwrap())?
                        .unwrap()
                        .reported_at
                        .is_none()
                );
                for sql in [
                    "SELECT count(*) FROM reports WHERE task_id = 3",
                    "SELECT count(*) FROM events WHERE kind = 'task.reported'",
                    "SELECT count(*) FROM wakes WHERE target_agent_id = 1",
                ] {
                    assert_eq!(conn.query_row(sql, [], |r| r.get::<_, i64>(0))?, 0, "{sql}");
                }
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(self.fill().await.tasks, [] as [TaskDetail; 0]);
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.scheduler.abort();
    }
}

#[tokio::test]
async fn four_answer_kinds_refuse_missing_or_template_then_deliver_and_surface() {
    for kind in [
        TaskKind::Lookup,
        TaskKind::Research,
        TaskKind::Reproduce,
        TaskKind::Teach,
    ] {
        let delivery = Delivery::new(kind).await;
        let answer = if kind == TaskKind::Reproduce {
            "reproduced: yes\nObserved the failure."
        } else {
            "The investigation is complete."
        };
        let mut args = json!({"outcome": "done", "answer": answer});
        if kind == TaskKind::Reproduce {
            args["reproduced"] = json!(true);
        }
        let refused = delivery.report(args.clone()).await;
        assert!(refused.is_error, "{kind}: {refused:?}");
        assert!(
            refused.text.contains("$QM_REPORT_DIR/report.html"),
            "{refused:?}"
        );
        delivery.assert_unreported().await;
        fs::copy(
            delivery.page().with_file_name("template.html"),
            delivery.page(),
        )
        .unwrap();
        let refused = delivery.report(args.clone()).await;
        assert!(
            refused.is_error && refused.text.contains("sample content"),
            "{refused:?}"
        );
        delivery.assert_unreported().await;
        delivery.write_page();
        // A valid but unsubmitted artifact is not a completed report in the UI.
        assert_eq!(delivery.fill().await.tasks, [] as [TaskDetail; 0]);
        let accepted = delivery.report(args).await;
        assert!(!accepted.is_error, "{kind}: {accepted:?}");
        assert!(accepted.text.contains("Your supervisor has it"));
        delivery.assert_delivered(kind, answer).await;
        let part = ReportPart::new(delivery.host.0.clone());
        assert_eq!(
            part.page(delivery.caller.task.unwrap()).await.unwrap(),
            delivery.page()
        );
        let mut snapshot = delivery.fill().await;
        assert_eq!(snapshot.tasks.len(), 1);
        assert!(snapshot.tasks[0].report_available);
        assert_eq!(snapshot.crew.len(), 1);
        assert!(!snapshot.crew[0].needs_you);
        assert_eq!(snapshot.actions, [] as [qm_types::api::ActionItem; 0]);
        snapshot.tasks[0].running = true;
        part.fill(&mut snapshot).await;
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.crew.len(), 1);
        assert!(snapshot.tasks[0].running, "fill preserves live task detail");
        // Retained artifact remains reachable through fill after the task leaves the live tree.
        delivery
            .fixture
            .db()
            .call(|conn| {
                conn.execute(
                    "UPDATE tasks SET status = 'closed', outcome = 'answered', closed_at = 2 WHERE id = 3",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(delivery.fill().await.tasks[0].report_available);
        fs::remove_file(delivery.page()).unwrap();
        assert_eq!(delivery.fill().await.tasks, [] as [TaskDetail; 0]);
        assert!(part.page(delivery.caller.task.unwrap()).await.is_err());
    }
}

#[tokio::test]
async fn html_does_not_replace_answer_or_reproduced_contract() {
    for reproduced in [false, true] {
        let delivery = Delivery::new(TaskKind::Reproduce).await;
        delivery.write_page();
        for args in [
            json!({"outcome": "done", "answer": "reproduced: yes"}),
            json!({"outcome": "done", "answer": "wrong first line", "reproduced": reproduced}),
            json!({"outcome": "done", "answer": if reproduced { "reproduced: no" } else { "reproduced: yes" }, "reproduced": reproduced}),
            json!({"outcome": "done", "reproduced": reproduced}),
        ] {
            let result = delivery.report(args).await;
            assert!(
                result.is_error && result.text.contains("reproduc"),
                "{result:?}"
            );
            delivery.assert_unreported().await;
        }
        let answer = if reproduced {
            "reproduced: yes\nSteps and evidence."
        } else {
            "reproduced: no\nTried the supplied steps."
        };
        let result = delivery
            .report(json!({"outcome": "done", "answer": answer, "reproduced": reproduced}))
            .await;
        assert!(!result.is_error, "{result:?}");
        delivery
            .fixture
            .db()
            .call(move |conn| {
                let reports = qm_store::reports::for_task(conn, TaskId::new(3).unwrap())?;
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].reproduced, Some(reproduced));
                assert_eq!(reports[0].answer.as_deref(), Some(answer));
                Ok(())
            })
            .await
            .unwrap();
    }
    let delivery = Delivery::new(TaskKind::Research).await;
    delivery.write_page();
    let result = delivery.report(json!({"outcome": "done"})).await;
    assert!(
        result.is_error && result.text.contains("answer"),
        "{result:?}"
    );
    delivery.assert_unreported().await;
}

#[tokio::test]
async fn unfinished_outcomes_and_other_kinds_do_not_require_html() {
    for kind in [
        TaskKind::Lookup,
        TaskKind::Research,
        TaskKind::Reproduce,
        TaskKind::Teach,
    ] {
        for outcome in ["blocked", "needs_decision"] {
            let delivery = Delivery::new(kind).await;
            let result = delivery
                .report(json!({"outcome": outcome, "blockers": ["Need input"]}))
                .await;
            assert!(!result.is_error, "{kind}/{outcome}: {result:?}");
            assert!(!delivery.page().exists());
            assert_eq!(delivery.fill().await.tasks, [] as [TaskDetail; 0]);
        }
    }
    let delivery = Delivery::new(TaskKind::Build).await;
    let result = delivery
        .report(json!({"outcome": "done", "answer": "No code changes necessary."}))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert!(!delivery.page().exists());
}
