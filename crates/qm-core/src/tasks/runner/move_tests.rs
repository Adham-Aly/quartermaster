//! A crewmate moved between its turns: its session closes, and reopens in the new folder resuming
//! the same conversation, on the Claude engine and on the provider engine alike.
use super::*;
use qm_engine::{AccountRef, Builtins, Engine, EngineError, SessionHandle, SessionSpec};
use qm_store::tasks::PendingMove;
use qm_types::domain::{DeliveryMode, EngineKind, Provider};
use qm_types::ids::{AccountId, ProjectId, SessionRef};
use std::path::PathBuf;
use std::sync::Mutex;

const CREWMATE: i64 = 3;

fn task() -> TaskId {
    TaskId::new(2).unwrap()
}

/// Records every spec it opens a session with; each session closes when asked.
#[derive(Debug)]
struct Recording {
    kind: EngineKind,
    specs: Arc<Mutex<Vec<SessionSpec>>>,
}
impl Engine for Recording {
    fn kind(&self) -> EngineKind {
        self.kind
    }
    fn open(&self, spec: SessionSpec) -> Result<SessionHandle, EngineError> {
        self.specs.lock().unwrap().push(spec);
        let (handle, mut side) = qm_engine::session_channel();
        tokio::spawn(async move {
            while let Some(command) = side.commands.recv().await {
                if matches!(
                    command,
                    qm_engine::SessionCommand::Close | qm_engine::SessionCommand::Forget
                ) {
                    let _ = side
                        .events
                        .send(EngineEvent::SessionClosed {
                            reason: CloseReason::Requested,
                        })
                        .await;
                    break;
                }
            }
        });
        Ok(handle)
    }
    fn models<'a>(
        &'a self,
        _: &'a AccountRef,
    ) -> qm_types::BoxFuture<'a, Result<Vec<qm_engine::DetectedModel>, EngineError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn usage<'a>(
        &'a self,
        _: &'a AccountRef,
    ) -> qm_types::BoxFuture<'a, Result<Option<qm_engine::UsageReading>, EngineError>> {
        Box::pin(async { Ok(None) })
    }
}

fn service_with(svc: &TaskService, engine: Arc<dyn Engine>) -> TaskService {
    let d = svc.deps();
    let mut engines = d.engines.clone();
    engines.insert(engine.kind(), engine);
    TaskService::new(crate::tasks::TaskDeps {
        db: d.db.clone(),
        clock: d.clock.clone(),
        data_dir: d.data_dir.clone(),
        home: d.home.clone(),
        engines,
        supervisor: d.supervisor.clone(),
        inbox: d.inbox.clone(),
        scheduler: d.scheduler.clone(),
        config: d.config.clone(),
        health: d.health.clone(),
        git: d.git.clone(),
        chat: d.chat.clone(),
        seat_agent: d.seat_agent,
        probe: d.probe.clone(),
        gate: d.gate.clone(),
        choices: d.choices.clone(),
        start_hook: d.start_hook.clone(),
        inherited_env: d.inherited_env.clone(),
        dispatcher: d.dispatcher,
        monitor: d.monitor.clone(),
    })
}

/// Crewmate C-2 (agent 3) works in place in `a`, with a stored session, and is moved to `b`.
async fn moved_session(kind: EngineKind) -> (SessionSpec, SessionSpec, PathBuf, PathBuf) {
    let fixture = crate::turns::forget_tests::Fixture::new().await;
    let (plain, scheduler) = fixture.task_service();
    let specs = Arc::new(Mutex::new(Vec::new()));
    let svc = service_with(
        &plain,
        Arc::new(Recording {
            kind,
            specs: specs.clone(),
        }),
    );
    let root = svc.deps().home.clone();
    let (a, b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let home = a.clone();
    fixture
        .db()
        .call(move |conn| {
            conn.execute(
                "UPDATE projects SET path = ?1, delivery_mode = 'folder' WHERE id = 1",
                [home.to_str().unwrap()],
            )?;
            conn.execute(
                "UPDATE tasks SET status = 'running', in_place = 1, delivery_mode = 'folder',
                 branch = NULL, port_base = 41040 WHERE id = 2",
                [],
            )?;
            conn.execute(
                "INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                 VALUES (?1, 'crewmate', 1, 2, ?2, 'idle', 1, 8)",
                rusqlite::params![CREWMATE, home.to_str().unwrap()],
            )?;
            qm_store::seq::observe_all(conn)?;
            let session = SessionRef::new("session-1").unwrap();
            agents::set_session(conn, AgentId::new(CREWMATE), Some(&session), Timestamp::UNIX_EPOCH)?;
            Ok(())
        })
        .await
        .unwrap();

    let (_tx, rx) = mpsc::unbounded_channel();
    let (activity, _) = broadcast::channel(64);
    let mut runner = CrewRunner::new(
        task(),
        AgentId::new(CREWMATE),
        svc.clone(),
        rx,
        Arc::new(RunnerState::new(activity)),
        RunnerStart::Resume,
    );
    runner.routed = Some((
        RoutedSession {
            engine: kind,
            account: AccountRef {
                id: AccountId::new(1),
                provider: if kind == EngineKind::Claude {
                    Provider::Claude
                } else {
                    Provider::Custom
                },
                credential: None,
                base_url: None,
            },
            model: ModelId::new("test-model").unwrap(),
            effort: qm_types::domain::Effort::Low,
        },
        PortBlock { base: 41040 },
    ));
    runner.ensure_session().await.unwrap();

    // The supervisor's move, as `set_task_project` records it for a crewmate with a runner.
    let to = PendingMove {
        project: ProjectId::new(1),
        in_place: true,
        delivery: DeliveryMode::Folder,
        home: b.clone(),
        branch: None,
        base_branch: None,
        base_ref: None,
        base_commit: None,
        verification: None,
        asked_at: 1,
    };
    fixture
        .db()
        .call(move |conn| qm_store::tasks::set_pending_move(conn, task(), Some(&to)).map(drop))
        .await
        .unwrap();
    runner.move_workplace().await;
    assert!(runner.session.is_none(), "the old session closed");
    runner.ensure_session().await.unwrap();

    let mut specs = specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 2, "one session before the move, one after");
    let after = specs.pop().unwrap();
    let before = specs.pop().unwrap();
    drop(runner);
    svc.shutdown().await;
    plain.shutdown().await;
    scheduler.abort();
    (before, after, a, b)
}

async fn check(kind: EngineKind) {
    let (before, after, a, b) = moved_session(kind).await;
    assert_eq!(before.cwd, a);
    assert_eq!(after.cwd, b, "the session reopens in the new folder");
    let session = Some(SessionRef::new("session-1").unwrap());
    assert_eq!(before.resume, session);
    assert_eq!(after.resume, session, "and resumes the same conversation");
    assert_eq!(
        after.session_dir, before.session_dir,
        "its own folder never moves"
    );
    assert_eq!(after.transcript_dir, before.transcript_dir);
    if let Builtins::ProviderFileTools { write_roots, .. } = &after.builtins {
        assert!(write_roots.contains(&b), "{write_roots:?}");
        assert!(
            !write_roots.contains(&a),
            "the old folder is no longer its own: {write_roots:?}"
        );
    }
}

#[tokio::test]
async fn a_moved_claude_crewmate_resumes_in_its_new_folder() {
    check(EngineKind::Claude).await;
}

#[tokio::test]
async fn a_moved_provider_crewmate_resumes_in_its_new_folder_and_writes_only_there() {
    check(EngineKind::Provider).await;
}
