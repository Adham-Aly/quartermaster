//! Regression tests for explicit compaction and closed-task forgetting.
use super::*;
use crate::turn_io::{IdleOutcome, Notice, OpenedSession, seat_inbox};
use qm_engine::{EngineSide, InitReport, SessionCommand, session_channel};
use qm_types::clock::SystemClock;
use qm_types::ids::SessionRef;

#[derive(Debug)]
struct Services;
impl ToolHost for Services {}
impl SeatSession for Services {
    fn open(&self, _: OpenReason) -> BoxFuture<'_, Result<OpenedSession, SeatError>> {
        Box::pin(async {
            Err(SeatError::Unavailable(
                "test has no replacement session".into(),
            ))
        })
    }
    fn session_ready<'a>(
        &'a self,
        _: AgentId,
        _: &'a SessionRef,
        _: bool,
        _: &'a InitReport,
    ) -> BoxFuture<'a, Result<(), SeatError>> {
        Box::pin(async { Ok(()) })
    }
    fn idle<'a>(
        &'a self,
        _: AgentId,
        _: &'a SessionHandle,
    ) -> BoxFuture<'a, Result<IdleOutcome, SeatError>> {
        Box::pin(async { Ok(IdleOutcome::Unchanged) })
    }
}
impl ContextSource for Services {
    fn context_block(&self, _: AgentId, _: TurnId) -> BoxFuture<'_, Result<String, SeatError>> {
        Box::pin(async { Ok(String::new()) })
    }
}
impl ChatLog for Services {
    fn message_text(&self, _: MessageId) -> BoxFuture<'_, Result<String, SeatError>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn waiting_messages(&self, _: AgentId) -> BoxFuture<'_, Result<Vec<MessageId>, SeatError>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn app_acknowledgement<'a>(
        &'a self,
        _: AgentId,
        _: TurnId,
        _: MessageId,
        _: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, SeatError>> {
        Box::pin(async { Ok(MessageId::new(999)) })
    }
    fn notice(
        &self,
        _: AgentId,
        _: TurnId,
        _: MessageId,
        _: Notice,
    ) -> BoxFuture<'_, Result<(), SeatError>> {
        Box::pin(async { Ok(()) })
    }
    fn app_note<'a>(
        &'a self,
        _: AgentId,
        _: Option<TurnId>,
        _: &'a str,
    ) -> BoxFuture<'a, Result<MessageId, SeatError>> {
        Box::pin(async { Ok(MessageId::new(999)) })
    }
    fn changed(&self) {}
}

pub(crate) struct Fixture {
    loop_: TurnLoop,
    side: EngineSide,
    _inbox: crate::turn_io::SeatInbox,
    dir: TempDir,
}
/// The fixture's folder, removed when the fixture goes.
pub(crate) struct TempDir(std::path::PathBuf);
impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Fixture {
    pub(crate) async fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "qm-turn-forget-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        // Report opening rejects symlinked ancestors (including macOS's /var alias).
        let dir = dir.canonicalize().unwrap();
        let db = Db::open(&dir.join("state.db")).unwrap();
        db.call(|conn| {
            conn.execute_batch("INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, merge_method, created_at, seq) VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
                INSERT INTO agents (id, role, home, status, created_at, seq) VALUES (1, 'quartermaster', '/h', 'idle', 1, 1);
                INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES (1, 1, 'you', 'message', 'old words', 1, 10);
                INSERT INTO turns (id, agent_id, started_by, message_id, started_at, ended_at, status, seq) VALUES (1, 1, 'user', 1, 1, 2, 'completed', 20);
                INSERT INTO session_lines (agent_id, turn_id, kind, text, created_at, seq) VALUES (1, 1, 'agent', 'old lines', 1, 21);
                INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, branch, status, outcome, created_at, closed_at, seq) VALUES (1, 1, 1, 'build', 'old title', 'old goal', 'direct_pr', 'qm/task-1', 'closed', 'landed', 1, 2, 1);
                INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, branch, status, created_at, seq) VALUES (2, 1, 1, 'build', 'open title', 'open goal', 'direct_pr', 'qm/task-2', 'queued', 1, 2);
                INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, retired_at, seq) VALUES (2, 'crewmate', 1, 1, '/c', 'stopped', 1, 2, 2);
                INSERT INTO reports (id, task_id, agent_id, source, answer, created_at, seq) VALUES (1, 1, 2, 'final_text', 'old answer', 1, 1);
                INSERT INTO turns (id, agent_id, started_by, started_at, ended_at, status, seq) VALUES (2, 2, 'app_event', 1, 2, 'completed', 3);
                INSERT INTO session_lines (agent_id, task_id, turn_id, kind, text, created_at, seq) VALUES (2, 1, 2, 'agent', 'crew lines', 1, 4);")?;
            qm_store::seq::observe_all(conn)?;
            Ok(())
        }).await.unwrap();
        let (inbox, rx) = seat_inbox();
        let mut loop_ = TurnLoop::new(
            TurnLoopDeps {
                db,
                inbox: crate::inbox::Inbox::new(),
                clock: Arc::new(SystemClock),
                seat: Arc::new(Services),
                context: Arc::new(Services),
                chat: Arc::new(Services),
                host: Arc::new(Services),
                dispatcher: Dispatcher::builtin(),
            },
            rx,
        );
        let (handle, side) = session_channel();
        loop_.agent = Some(AgentId::new(1));
        loop_.session = Some(Live {
            agent: AgentId::new(1),
            handle,
            preamble: None,
            model: None,
        });
        Self {
            loop_,
            side,
            _inbox: inbox,
            dir: TempDir(dir),
        }
    }
    /// The fixture's database.
    pub(crate) fn db(&self) -> &Db {
        &self.loop_.deps.db
    }
    pub(crate) fn task_service(&self) -> (crate::tasks::TaskService, tokio::task::JoinHandle<()>) {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let (scheduler, job) = crate::scheduler::Scheduler::spawn(clock.clone());
        let (_, config) = tokio::sync::watch::channel(qm_types::config::Config::default());
        let (_, setup) = tokio::sync::watch::channel(crate::setup_check::SetupFacts::default());
        let svc = crate::tasks::TaskService::wire(crate::tasks::TaskWiring {
            db: self.loop_.deps.db.clone(),
            clock,
            data_dir: qm_types::data_dir::DataDir::with_scratch(
                self.dir.join("home"),
                self.dir.join("scratch"),
            ),
            home: self.dir.to_path_buf(),
            engines: std::collections::BTreeMap::new(),
            supervisor: Arc::new(qm_engine::supervisor::OsSupervisor::new(None)),
            inbox: crate::inbox::Inbox::new(),
            scheduler,
            config,
            health: Arc::new(qm_auth::LoginHealth::new()),
            chat: Arc::new(Services),
            seat_agent: AgentId::new(1),
            setup,
            probe: None,
            monitor: crate::context_monitor::ContextMonitor::new(Arc::new(SystemClock)),
        });
        (svc, job)
    }
    async fn assert_forgotten(&self) {
        let (svc, scheduler) = self.task_service();
        let caller = qm_tools::handler::Caller {
            agent: AgentId::new(1),
            role: Role::Quartermaster,
            task: None,
            secondmate: None,
            turn: None,
            plan: None,
        };
        let task = qm_types::ids::TaskId::new(1).unwrap();
        let qm_tools::handler::ToolFailure::Refused(refusal) =
            svc.task_detail(&caller, task).await.unwrap_err()
        else {
            panic!("expected a forgotten-task refusal");
        };
        let refusal = refusal.reason;
        assert!(
            refusal.contains("forgotten") && refusal.contains("find_tasks"),
            "{refusal}"
        );
        let request = qm_tools::handler::StartRequest {
            folder: None,
            project: Some("p".into()),
            in_place: false,
            kind: qm_types::domain::TaskKind::Build,
            title: "new".into(),
            goal: "new".into(),
            constraints: vec![],
            tests_required: false,
            builds_on: Some(task),
            instructions: vec![],
            later_steps: vec![],
            model: None,
            preference: None,
            effort: None,
            premium: false,
            choice_message: None,
            base_branch: None,
            verification: None,
            stronger_retry_of: None,
            correlation: None,
        };
        let qm_tools::handler::ToolFailure::Refused(refusal) =
            svc.start_crewmate(&caller, request).await.unwrap_err()
        else {
            panic!("expected a forgotten builds-on refusal");
        };
        let refusal = refusal.reason;
        assert!(
            refusal.contains("can't build on") && refusal.contains("forgotten"),
            "{refusal}"
        );
        drop(svc);
        scheduler.abort();
        let _ = scheduler.await;
        self.loop_
            .deps
            .db
            .call(|conn| {
                for table in ["reports", "session_lines", "turns"] {
                    let n: i64 =
                        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                    assert_eq!(n, 0, "{table} forgotten");
                }
                let tasks: Vec<u32> = conn
                    .prepare("SELECT id FROM tasks")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                assert_eq!(tasks, [2], "open task stays, closed task goes");
                assert!(
                    qm_store::ids::fresh(conn, "tasks", 1..=1).is_err(),
                    "forgotten number stays reserved"
                );
                assert_eq!(
                    conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
                        .get::<_, i64>(
                        0
                    ))?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn explicit_compaction_forgets_old_rows_but_preserves_queued_inputs() {
    let mut f = Fixture::new().await;
    f.loop_
        .deps
        .db
        .call(|conn| {
            conn.execute_batch(
                "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES
            (2, 1, 'you', 'message', 'queued behind slash while the last turn ran', 1, 30),
            (4, 1, 'agent', 'reply', 'old reply after queued input', 1, 40);",
            )?;
            qm_store::seq::observe_all(conn)?;
            Ok(())
        })
        .await
        .unwrap();
    f.loop_.queue.push_user(MessageId::new(2));
    let db = f.loop_.deps.db.clone();
    let side = &mut f.side;
    let engine = async move {
        assert_eq!(side.commands.recv().await, Some(SessionCommand::Compact));
        db.call(|conn| {
            let seq = qm_store::seq::next(jiff::Timestamp::from_second(1).unwrap());
            conn.execute("INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES (3, 1, 'you', 'message', 'queued during compaction', 1, ?1)", [seq])?;
            Ok(())
        }).await.unwrap();
        side.events
            .send(EngineEvent::Compacted {
                automatic: false,
                tokens_before: Some(100),
                tokens_after: Some(10),
            })
            .await
            .unwrap();
        side.events
            .send(EngineEvent::TurnEnded {
                outcome: qm_engine::TurnOutcome::Completed,
                usage: qm_engine::TurnUsage::default(),
                final_text: String::new(),
            })
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(
        f.loop_.run_slash(qm_types::api::SlashCommand::Compact),
        engine
    );
    result.unwrap();
    f.assert_forgotten().await;
    f.loop_
        .deps
        .db
        .call(|conn| {
            let messages: Vec<u32> = conn
                .prepare("SELECT id FROM messages")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert_eq!(messages, [2, 3]);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn automatic_compaction_during_b_keeps_b_and_previously_queued_c() {
    let mut f = Fixture::new().await;
    f.loop_
        .deps
        .db
        .call(|conn| {
            conn.execute_batch(
                "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq) VALUES
                 (2, 1, 'you', 'message', 'B queued while A ran', 1, 30),
                 (3, 1, 'you', 'message', 'C queued while A ran', 1, 40),
                 (4, 1, 'agent', 'reply', 'A reply after B and C', 1, 50);",
            )?;
            qm_store::seq::observe_all(conn)?;
            Ok(())
        })
        .await
        .unwrap();
    f.loop_.queue.push_user(MessageId::new(2));
    f.loop_.queue.push_user(MessageId::new(3));
    let side = &mut f.side;
    let engine = async {
        for attempt in 0..2 {
            // With no tool effects, the turn policy asks for one retry before its notice.
            assert!(matches!(
                side.commands.recv().await,
                Some(SessionCommand::StartTurn { .. })
            ));
            side.events.send(EngineEvent::TurnStarted).await.unwrap();
            if attempt == 0 {
                side.events
                    .send(EngineEvent::Compacted {
                        automatic: true,
                        tokens_before: Some(100),
                        tokens_after: Some(10),
                    })
                    .await
                    .unwrap();
            }
            side.events
                .send(EngineEvent::TurnEnded {
                    outcome: qm_engine::TurnOutcome::Completed,
                    usage: qm_engine::TurnUsage::default(),
                    final_text: String::new(),
                })
                .await
                .unwrap();
        }
    };
    let run = async {
        f.loop_.run_turn(engine_turn::Start::User).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run, engine);
    })
    .await
    .expect("B's turn and automatic compaction finish");
    assert_eq!(f.loop_.queue.queued(), vec![MessageId::new(3)]);
    f.loop_
        .deps
        .db
        .call(|conn| {
            let messages: Vec<u32> = conn
                .prepare("SELECT id FROM messages ORDER BY id")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert_eq!(
                messages,
                [2, 3],
                "current B and unseen C stay; A and its reply go"
            );
            let (input, cutoff): (MessageId, i64) = conn.query_row(
                "SELECT message_id, seq FROM turns WHERE agent_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(input, MessageId::new(2));
            assert!(cutoff > 40, "both queued inputs predate B's turn");
            assert_eq!(
                qm_store::messages::waiting(conn, AgentId::new(1))?,
                vec![MessageId::new(3)]
            );
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM session_lines WHERE text = 'old lines'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r
                    .get::<_, i64>(
                    0
                ))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn clear_forgets_closed_tasks_reports_and_crew_history() {
    let mut f = Fixture::new().await;
    let side = &mut f.side;
    let engine = async move {
        assert_eq!(side.commands.recv().await, Some(SessionCommand::Forget));
        side.events
            .send(EngineEvent::SessionClosed {
                reason: CloseReason::Requested,
            })
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(
        f.loop_.run_slash(qm_types::api::SlashCommand::Clear),
        engine
    );
    result.unwrap();
    f.assert_forgotten().await;
}

#[tokio::test]
async fn successful_forgetting_signals_minimal_fact_recipients() {
    // A secondmate's parent may be the seat or another secondmate. Neither should need a
    // restart or an unrelated user message to discover facts inserted directly by history.
    for command in [
        qm_types::api::SlashCommand::Clear,
        qm_types::api::SlashCommand::Compact,
    ] {
        for target in [AgentId::new(1), AgentId::new(4)] {
            let mut f = Fixture::new().await;
            f.loop_.deps.db.call(move |conn| {
                conn.execute_batch("INSERT INTO secondmates (id, name, scope, created_at, seq) VALUES
                    (1, 'child', 'child', 0, 1), (2, 'parent', 'parent', 0, 2);
                    INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, created_at, seq)
                    VALUES (4, 'secondmate', 1, 2, '/parent', 'idle', 0, 4);")?;
                conn.execute("INSERT INTO agents (id, role, supervisor_id, secondmate_id, home, status, created_at, seq)
                    VALUES (3, 'secondmate', ?1, 1, '/child', 'idle', 0, 3)", [target])?;
                conn.execute_batch("INSERT INTO secondmate_requests (id, secondmate_id, request, status, created_at, seq)
                    VALUES (1, 1, 'private request', 'answered', 0, 1);")?;
                conn.execute("UPDATE tasks SET supervisor_id = 3, secondmate_request_id = 1 WHERE id = 1", [])?;
                conn.execute_batch("INSERT INTO events (id, at, kind, task_id, summary, payload, seq)
                    VALUES (10, 0, 'task.closed', 1, 'private details', '{\"outcome\":\"landed\"}', 50);")?;
                qm_store::seq::observe_all(conn)?;
                Ok(())
            }).await.unwrap();
            let (recipient, mut rx) = seat_inbox();
            let sink: Arc<dyn crate::inbox::WakeSink> = Arc::new(crate::inbox::SeatSink(recipient));
            f.loop_.deps.inbox.register(target, &sink);
            assert!(
                matches!(rx.inputs.try_recv(), Ok(SeatInput::Wake)),
                "registration catchup"
            );
            let side = &mut f.side;
            let engine = async {
                match side.commands.recv().await.unwrap() {
                    SessionCommand::Forget => {
                        side.events
                            .send(EngineEvent::SessionClosed {
                                reason: CloseReason::Requested,
                            })
                            .await
                            .unwrap();
                    }
                    SessionCommand::Compact => {
                        side.events
                            .send(EngineEvent::Compacted {
                                automatic: false,
                                tokens_before: Some(100),
                                tokens_after: Some(10),
                            })
                            .await
                            .unwrap();
                        side.events
                            .send(EngineEvent::TurnEnded {
                                outcome: qm_engine::TurnOutcome::Completed,
                                usage: qm_engine::TurnUsage::default(),
                                final_text: String::new(),
                            })
                            .await
                            .unwrap();
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            };
            let (result, ()) = tokio::join!(f.loop_.run_slash(command), engine);
            result.unwrap();
            assert!(
                matches!(rx.inputs.try_recv(), Ok(SeatInput::Wake)),
                "recipient signalled after forgetting"
            );
            f.loop_.deps.db.call(move |conn| {
                let (actual, task, payload): (AgentId, Option<i64>, String) = conn.query_row(
                    "SELECT target_agent_id, task_id, payload FROM wakes WHERE dedupe_key = 'forgotten-parent:10'",
                    [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                assert_eq!(actual, target);
                assert_eq!(task, None, "fact survives deletion of its task");
                assert!(!payload.contains("private"));
                Ok(())
            }).await.unwrap();
        }
    }
}

#[tokio::test]
async fn unsuccessful_explicit_compaction_does_not_forget() {
    let mut f = Fixture::new().await;
    f.side
        .events
        .send(EngineEvent::TurnEnded {
            outcome: qm_engine::TurnOutcome::Interrupted,
            usage: qm_engine::TurnUsage::default(),
            final_text: String::new(),
        })
        .await
        .unwrap();
    f.loop_
        .run_slash(qm_types::api::SlashCommand::Compact)
        .await
        .unwrap();
    f.loop_
        .deps
        .db
        .call(|conn| {
            assert!(qm_store::tasks::load(conn, qm_types::ids::TaskId::new(1).unwrap())?.is_some());
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM turns", [], |r| r.get::<_, i64>(0))?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
}

/// Runs the loop until `input` ends it, answering the one command it sends with `SessionClosed`.
async fn the_command_that_ends(input: SeatInput) -> Option<SessionCommand> {
    let Fixture {
        loop_,
        mut side,
        _inbox: inbox,
        dir: _dir,
    } = Fixture::new().await;
    inbox.send(input).unwrap();
    let engine = async move {
        let command = side.commands.recv().await;
        side.events
            .send(EngineEvent::SessionClosed {
                reason: CloseReason::Requested,
            })
            .await
            .unwrap();
        command
    };
    let (result, command) = tokio::join!(loop_.run(), engine);
    result.unwrap();
    command
}

#[tokio::test]
async fn quitting_closes_the_session_so_its_transcript_resumes_at_the_next_start() {
    assert_eq!(
        the_command_that_ends(SeatInput::Shutdown).await,
        Some(SessionCommand::Close)
    );
}

#[tokio::test]
async fn retiring_forgets_the_session() {
    assert_eq!(
        the_command_that_ends(SeatInput::Retire).await,
        Some(SessionCommand::Forget)
    );
}
