//! The session against a fake `claude` (`fake_claude.py`), which plays the message orders seen
//! from the real one (claude 2.1.309) and records what the engine writes to it.

use super::Session;
use crate::detect::{ClaudeInstall, LoginState};
use crate::{ClaudeConfig, ClaudeEngine};
use qm_engine::supervisor::OsSupervisor;
use qm_engine::{
    AccountRef, Builtins, Engine, EngineEvent, SessionHandle, SessionSpec, SystemPrompt, TurnInput,
    session_channel,
};
use qm_types::clock::SystemClock;
use qm_types::config::Config;
use qm_types::domain::{Effort, Provider, Role};
use qm_types::ids::{AccountId, AgentId, JobLabel, ModelId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const FAKE: &str = include_str!("fake_claude.py");
/// How long a test waits for an event it expects.
const PATIENCE: Duration = Duration::from_secs(10);
/// How long a test watches for events it expects never to come.
const QUIET: Duration = Duration::from_millis(600);

/// A throwaway folder with the fake `claude`, its scenario and its record of stdin.
struct Fake {
    dir: PathBuf,
    _settings: watch::Sender<Config>,
    engine: ClaudeEngine,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("the tests need PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|p| p.is_file())
        .expect("the session tests need python3 on PATH")
}

impl Fake {
    fn new(scenario: &Value) -> Self {
        let dir = std::env::temp_dir().join(format!("qm-fake-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("session")).unwrap();
        std::fs::write(dir.join("fake_claude.py"), FAKE).unwrap();
        std::fs::write(dir.join("scenario.json"), scenario.to_string()).unwrap();
        let claude = dir.join("claude");
        std::fs::write(
            &claude,
            format!(
                "#!/bin/sh\nexec '{}' '{}' '{}' '{}'\n",
                python3().display(),
                dir.join("fake_claude.py").display(),
                dir.join("scenario.json").display(),
                dir.join("record.jsonl").display(),
            ),
        )
        .unwrap();
        set_executable(&claude);
        let mut inherited_env = BTreeMap::new();
        if let Some(path) = std::env::var_os("PATH") {
            inherited_env.insert("PATH".to_owned(), path.to_string_lossy().into_owned());
        }
        inherited_env.insert("HOME".to_owned(), dir.display().to_string());
        let (settings_tx, settings) = watch::channel(Config::default());
        let engine = ClaudeEngine::new(ClaudeConfig {
            claude: claude.clone(),
            supervisor: Arc::new(OsSupervisor::new(None)),
            clock: Arc::new(SystemClock),
            inherited_env,
            settings,
        });
        engine.set_install(ClaudeInstall {
            path: Some(claude),
            version: Some("2.1.309".into()),
            login: LoginState::Subscription {
                subscription_type: "max".into(),
            },
        });
        Self {
            dir,
            _settings: settings_tx,
            engine,
        }
    }

    fn spec(&self) -> SessionSpec {
        let mut spec = SessionSpec::new(
            AgentId::new(7),
            Role::Crewmate,
            AccountRef {
                id: AccountId::new(1),
                provider: Provider::Claude,
                credential: None,
                base_url: None,
            },
            ModelId::new("claude-haiku-5-5").unwrap(),
            Effort::Low,
            self.dir.clone(),
            self.dir.join("session"),
            SystemPrompt::AppendToClaudeCode {
                text: "test".into(),
            },
        );
        spec.builtins = Builtins::ClaudeCode;
        spec
    }

    fn open(&self) -> SessionHandle {
        self.engine.open(self.spec()).unwrap()
    }

    /// The control requests the engine sent, by subtype.
    fn requests(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("record.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|m| {
                m.pointer("/request/subtype")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ---- What the fake says ----

fn line(msg: &Value) -> Value {
    json!({ "line": msg })
}

fn sleep(ms: u64) -> Value {
    json!({ "sleep_ms": ms })
}

fn init() -> Value {
    line(&json!({"type": "system", "subtype": "init", "tools": [],
                "capabilities": ["interrupt_receipt_v1"]}))
}

fn running() -> Value {
    line(&json!({"type": "system", "subtype": "session_state_changed", "state": "running"}))
}

fn text(text: &str) -> Vec<Value> {
    let event = |e: Value| line(&json!({"type": "stream_event", "event": e}));
    vec![
        event(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
        ),
        event(json!({"type": "content_block_delta", "index": 0,
                     "delta": {"type": "text_delta", "text": text}})),
        event(json!({"type": "content_block_stop", "index": 0})),
    ]
}

fn result() -> Value {
    line(
        &json!({"type": "result", "subtype": "success", "is_error": false,
                "usage": {"input_tokens": 1, "output_tokens": 1}, "total_cost_usd": 0.0}),
    )
}

fn helper_started(task: &str) -> Value {
    line(
        &json!({"type": "system", "subtype": "task_started", "task_id": task,
                "subagent_type": "general-purpose", "is_backgrounded": true,
                "description": format!("{task} helper")}),
    )
}

fn task_ended(task: &str, status: &str) -> Value {
    line(
        &json!({"type": "system", "subtype": "task_notification", "task_id": task,
                "status": status, "summary": format!("{task}-done")}),
    )
}

/// The crewmate's turn: it starts helpers FAST and SLOW in the background and ends.
fn launch_turn() -> Vec<Value> {
    let mut steps = vec![init(), helper_started("FAST"), helper_started("SLOW")];
    steps.extend(text("launched."));
    steps.push(result());
    steps
}

fn helper(task: &str) -> JobLabel {
    JobLabel::helper(task).unwrap()
}

// ---- Reading events ----

/// The events these tests look at; the rest (activity, usage, context) are dropped.
fn kept(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::TurnStarted
            | EngineEvent::TextLine { .. }
            | EngineEvent::TurnEnded { .. }
            | EngineEvent::JobStarted { .. }
            | EngineEvent::JobFinished { .. }
            | EngineEvent::Suspended
            | EngineEvent::SessionClosed { .. }
    )
}

/// The kept events until (and with) the first that `last` matches.
async fn until(
    handle: &mut SessionHandle,
    last: impl Fn(&EngineEvent) -> bool,
) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(PATIENCE, handle.next_event())
            .await
            .unwrap_or_else(|_| panic!("timed out; so far: {out:?}"))
            .unwrap_or_else(|| panic!("the session ended; so far: {out:?}"));
        if !kept(&event) {
            continue;
        }
        let done = last(&event);
        out.push(event);
        if done {
            return out;
        }
    }
}

/// The kept events within [`QUIET`].
async fn quiet(handle: &mut SessionHandle) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + QUIET;
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, handle.next_event()).await {
        if kept(&event) {
            out.push(event);
        }
    }
    out
}

async fn ready(handle: &mut SessionHandle) {
    let event = tokio::time::timeout(PATIENCE, handle.next_event())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(event, EngineEvent::SessionReady { .. }),
        "{event:?}"
    );
}

fn ended(event: &EngineEvent) -> bool {
    matches!(event, EngineEvent::TurnEnded { .. })
}

fn text_line(text: &str) -> EngineEvent {
    EngineEvent::TextLine { text: text.into() }
}

fn started(job: &JobLabel) -> impl Fn(&EngineEvent) -> bool + '_ {
    move |e| matches!(e, EngineEvent::JobStarted { job: j, .. } if j == job)
}

fn finished(
    job: &JobLabel,
    exit_code: Option<i32>,
    agent_told: bool,
) -> impl Fn(&EngineEvent) -> bool + '_ {
    move |e| {
        matches!(e, EngineEvent::JobFinished { job: j, exit_code: c, agent_told: t, .. }
            if j == job && *c == exit_code && *t == agent_told)
    }
}

/// Runs the launch turn and checks it: both helpers started, the turn ended.
async fn launched(handle: &mut SessionHandle) {
    ready(handle).await;
    handle
        .start_turn(TurnInput::text("start two helpers"))
        .unwrap();
    let events = until(handle, ended).await;
    assert_eq!(events[0], EngineEvent::TurnStarted);
    assert!(events.iter().any(started(&helper("FAST"))), "{events:?}");
    assert!(events.iter().any(started(&helper("SLOW"))), "{events:?}");
    assert!(events.contains(&text_line("launched.")), "{events:?}");
}

// ---- Tests ----

/// A helper ends while the crewmate is idle and another still runs: `claude` starts a turn by
/// itself, which is adopted, never interrupted, so the other helper goes on running.
#[tokio::test]
async fn a_turn_claude_starts_by_itself_is_adopted_not_interrupted() {
    let mut turn = launch_turn();
    turn.extend([sleep(200), task_ended("FAST", "completed"), init()]);
    turn.extend(text("FAST is done."));
    turn.push(result());
    let fake = Fake::new(&json!({ "turns": [turn] }));
    let mut handle = fake.open();
    launched(&mut handle).await;

    let events = until(&mut handle, ended).await;
    assert!(
        finished(&helper("FAST"), Some(0), true)(&events[0]),
        "{events:?}"
    );
    assert_eq!(events[1], EngineEvent::TurnStarted);
    assert_eq!(events[2], text_line("FAST is done."));
    assert!(ended(&events[3]), "{events:?}");
    assert_eq!(events.len(), 4, "{events:?}");

    let later = quiet(&mut handle).await;
    assert!(later.is_empty(), "SLOW must keep running: {later:?}");
    assert!(
        !fake.requests().iter().any(|r| r == "interrupt"),
        "{:?}",
        fake.requests()
    );

    // SLOW dies with the session; the model never heard of that.
    handle.close().unwrap();
    let events = until(&mut handle, |e| {
        matches!(e, EngineEvent::SessionClosed { .. })
    })
    .await;
    assert!(
        finished(&helper("SLOW"), None, false)(&events[0]),
        "{events:?}"
    );
}

/// `session_state_changed: running` before the `system/init`: one turn, adopted at the first.
#[tokio::test]
async fn either_message_first_adopts_the_turn_once() {
    let mut turn = launch_turn();
    turn.extend([
        sleep(200),
        task_ended("FAST", "completed"),
        running(),
        init(),
    ]);
    turn.extend(text("FAST is done."));
    turn.push(result());
    let fake = Fake::new(&json!({ "turns": [turn] }));
    let mut handle = fake.open();
    launched(&mut handle).await;

    let events = until(&mut handle, ended).await;
    let starts = events
        .iter()
        .filter(|e| **e == EngineEvent::TurnStarted)
        .count();
    assert_eq!(starts, 1, "{events:?}");
    assert!(events.contains(&text_line("FAST is done.")), "{events:?}");
    assert_eq!(quiet(&mut handle).await, Vec::<EngineEvent>::new());
    assert!(!fake.requests().iter().any(|r| r == "interrupt"));
}

/// A `system/init` while the process is still starting is neither adopted nor interrupted.
#[tokio::test]
async fn an_init_while_starting_is_left_alone() {
    let mut turn = vec![init()];
    turn.extend(text("hello"));
    turn.push(result());
    let fake = Fake::new(&json!({ "on_initialize": [init()], "turns": [turn] }));
    let mut handle = fake.open();
    ready(&mut handle).await;
    assert_eq!(quiet(&mut handle).await, Vec::<EngineEvent>::new());
    assert!(!fake.requests().iter().any(|r| r == "interrupt"));

    handle.start_turn(TurnInput::text("hi")).unwrap();
    let events = until(&mut handle, ended).await;
    assert_eq!(events[0], EngineEvent::TurnStarted);
    assert_eq!(events[1], text_line("hello"));
}

/// A closing session starts no new work: a turn `claude` starts then is interrupted.
#[tokio::test]
async fn a_closing_session_interrupts_a_self_started_turn() {
    let fake = Fake::new(&json!({ "turns": [] }));
    let (mut handle, side) = session_channel();
    let mut session = Session::new(Arc::clone(&fake.engine.shared), fake.spec(), side);
    session.start(false).await.unwrap();
    session.closing = true;
    let msg = json!({"type": "system", "subtype": "init", "tools": [],
                     "capabilities": ["interrupt_receipt_v1"]});
    session.on_system_init(&msg).await;
    assert!(session.turn.is_none());

    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !fake.requests().iter().any(|r| r == "interrupt") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            fake.requests()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.stop_process().await;
    drop(session);
    let mut rest = Vec::new();
    while let Some(event) = handle.next_event().await {
        rest.push(event);
    }
    assert!(!rest.contains(&EngineEvent::TurnStarted), "{rest:?}");
}

/// A process that exits with a helper running reports it finished, unheard of by the model.
#[tokio::test]
async fn a_job_that_dies_with_the_process_was_not_told() {
    let mut turn = launch_turn();
    turn.extend([sleep(200), json!({ "exit": 1 })]);
    let fake = Fake::new(&json!({ "turns": [turn] }));
    let mut handle = fake.open();
    launched(&mut handle).await;

    let events = until(
        &mut handle,
        |e| matches!(e, EngineEvent::JobFinished { job, .. } if *job == helper("SLOW")),
    )
    .await;
    assert!(
        finished(&helper("FAST"), None, false)(&events[0]),
        "{events:?}"
    );
    assert!(
        finished(&helper("SLOW"), None, false)(&events[1]),
        "{events:?}"
    );
}
