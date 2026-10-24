//! Deterministic, current-thread loopback regressions for request-boundary command intake.
//! Only JSON bodies are captured: even synthetic authorization headers are never retained.
use super::*;
use crate::builtin::{BuiltinOutcome, JobNotice};
use crate::model::{ModelProfile, ModelProfiles};
use crate::session_store::SessionStore;
use crate::transcript::Message;
use qm_auth::{CredentialStore, LoginEndpoints, LoginHealth};
use qm_engine::{
    EngineEvent, SessionCommand, StopReason, ToolCallId, ToolDescriptor, ToolResult, TurnInput,
    TurnOutcome, session_channel, supervisor::OsSupervisor,
};
use qm_types::{
    clock::SystemClock,
    domain::{Effort, Role},
    ids::{AgentId, ModelId, SessionRef},
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct TestCredentials {
    // Commands issued synchronously by successive credential reads, before the snapshot.
    on_read: Mutex<VecDeque<Vec<SessionCommand>>>,
    commands: Mutex<Option<mpsc::UnboundedSender<SessionCommand>>>,
}

impl CredentialStore for TestCredentials {
    fn get(&self, _: &CredentialRef) -> Result<Option<Credential>, AuthError> {
        if let Some(commands) = self.on_read.lock().unwrap().pop_front() {
            for command in commands {
                self.commands
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .send(command)
                    .unwrap();
            }
        }
        Ok(Some(Credential::ApiKey {
            key: Secret::new("test-only"),
        }))
    }
    fn put(&self, _: &CredentialRef, _: Credential) -> Result<(), AuthError> {
        panic!("test must not save credentials")
    }
    fn remove(&self, _: &CredentialRef) -> Result<(), AuthError> {
        panic!("test must not remove credentials")
    }
}

#[derive(Debug)]
struct TestProfiles;
impl ModelProfiles for TestProfiles {
    fn profile(&self, _: Provider, model: &ModelId) -> ModelProfile {
        let mut profile = ModelProfile::unknown(model.clone());
        profile.context_window = 40_000;
        profile.reasoning = false;
        profile
    }
    fn list(&self, _: Provider) -> Vec<ModelProfile> {
        Vec::new()
    }
}

struct Request {
    body: Value,
    reply: oneshot::Sender<(u16, String)>,
}
impl Request {
    fn respond(self, body: String) {
        self.reply.send((200, body)).unwrap();
    }
}

struct Fixture {
    commands: mpsc::UnboundedSender<SessionCommand>,
    handle: SessionHandle,
    requests: mpsc::UnboundedReceiver<Request>,
    server: JoinHandle<()>,
    session: JoinHandle<()>,
    credentials: Arc<TestCredentials>,
    id: SessionRef,
    events: Vec<EngineEvent>,
}

async fn loopback() -> (String, mpsc::UnboundedReceiver<Request>, JoinHandle<()>) {
    // Respect the developer run's reserved ports; CI uses OS-assigned loopback ports.
    let base = std::env::var("QM_PORT_BASE")
        .ok()
        .map(|s| s.parse::<u16>().unwrap());
    let ports = base.map_or(0..=0, |base| base..=base + 9);
    let mut listener = None;
    for port in ports {
        if let Ok(bound) =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
        {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("an available loopback test port");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (body_start, len) = loop {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0, "request ended before its headers");
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                    let len: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    break (end + 4, len);
                }
            };
            while bytes.len() < body_start + len {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0, "request ended before its body");
                bytes.extend_from_slice(&buf[..n]);
            }
            let body = serde_json::from_slice(&bytes[body_start..body_start + len]).unwrap();
            let (reply, response) = oneshot::channel();
            if requests_tx.send(Request { body, reply }).is_err() {
                break;
            }
            let Ok((status, body)) = response.await else {
                break;
            };
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            // A cancellation may close the client before its gated response is released.
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (base_url, requests, server)
}

impl Fixture {
    async fn new(builtin: bool) -> Self {
        let (base_url, requests, server) = loopback().await;
        let credentials = Arc::new(TestCredentials::default());
        let (_, settings) = watch::channel(Config::default());
        let engine = ProviderEngine::new(ProviderEngineConfig {
            tokens: Arc::new(TokenKeeper::new(
                credentials.clone(),
                LoginEndpoints::default(),
                Arc::new(LoginHealth::new()),
            )),
            supervisor: Arc::new(OsSupervisor::new(None)),
            clock: Arc::new(SystemClock),
            models: Arc::new(TestProfiles),
            builtins: None,
            inherited_env: BTreeMap::new(),
            settings,
            user_agent: "quartermaster-test".into(),
        });
        let mut spec = SessionSpec::new(
            AgentId::new(1),
            Role::Crewmate,
            AccountRef {
                id: AccountId::new(1),
                provider: Provider::Custom,
                credential: Some("test:steering".into()),
                base_url: Some(base_url),
            },
            ModelId::new("steering-test").unwrap(),
            Effort::Medium,
            "/unused-test".into(),
            "/unused-test/session".into(),
            SystemPrompt::Replace {
                text: "test system".into(),
            },
        );
        spec.tools = vec![descriptor("ours")];
        let (handle, mut side) = session_channel();
        // Use the engine's real session driver, with a cloneable command sender for hooks
        // that enqueue inside credential lookup / builtin completion without a scheduling gap.
        let (commands, rx) = mpsc::unbounded_channel();
        side.commands = rx;
        *credentials.commands.lock().unwrap() = Some(commands.clone());
        let builtins =
            builtin.then(|| Arc::new(BoundaryBuiltin(commands.clone())) as Arc<dyn BuiltinTools>);
        let session = tokio::spawn(session::supervise(engine.shared, spec, side, builtins, 1));
        let mut f = Self {
            commands,
            handle,
            requests,
            server,
            session,
            credentials,
            id: SessionRef::new("pending").unwrap(),
            events: Vec::new(),
        };
        match f.event().await {
            EngineEvent::SessionReady { session, .. } => f.id = session,
            other => panic!("expected ready, got {other:?}"),
        }
        f
    }

    fn send(&self, command: SessionCommand) {
        self.commands.send(command).unwrap();
    }
    fn start(&self) {
        self.send(SessionCommand::StartTurn {
            input: TurnInput::text("start"),
        });
    }
    fn inject(&self, text: &str) {
        self.send(inject(text));
    }
    fn result(&self, id: &str, text: &str) {
        self.send(SessionCommand::ToolResult {
            call: ToolCallId::new(id),
            result: ToolResult::ok(text),
        });
    }
    async fn request(&mut self) -> Request {
        tokio::time::timeout(WAIT, self.requests.recv())
            .await
            .expect("no model request")
            .unwrap()
    }
    async fn event(&mut self) -> EngineEvent {
        let event = tokio::time::timeout(WAIT, self.handle.next_event())
            .await
            .expect("no engine event")
            .unwrap();
        self.events.push(event.clone());
        event
    }
    async fn tool_calls(&mut self, n: usize) {
        let mut found = 0;
        while found < n {
            match self.event().await {
                EngineEvent::ToolCall { .. } => found += 1,
                EngineEvent::TurnEnded { outcome, .. } => panic!("ended before tools: {outcome:?}"),
                _ => {}
            }
        }
    }
    async fn ended(&mut self, expected: TurnOutcome, calls: u32) {
        loop {
            if let EngineEvent::TurnEnded { outcome, usage, .. } = self.event().await {
                assert_eq!(outcome, expected);
                assert_eq!(usage.model_calls, calls);
                break;
            }
        }
    }
    async fn close(&mut self) {
        self.send(SessionCommand::Close);
        self.closed().await;
    }
    async fn closed(&mut self) {
        while !matches!(self.event().await, EngineEvent::SessionClosed { .. }) {}
        tokio::time::timeout(WAIT, &mut self.session)
            .await
            .expect("session did not stop")
            .unwrap();
        assert_eq!(
            self.events
                .iter()
                .filter(|e| matches!(e, EngineEvent::TurnStarted))
                .count(),
            1
        );
        assert_eq!(
            self.events
                .iter()
                .filter(|e| matches!(e, EngineEvent::TurnEnded { .. }))
                .count(),
            1
        );
        assert!(
            self.requests.try_recv().is_err(),
            "unexpected extra model request"
        );
    }
    fn messages(&self) -> Vec<Message> {
        SessionStore::resume(self.id.as_str(), None)
            .unwrap()
            .entries
            .into_iter()
            .filter_map(|entry| {
                if let crate::session_store::Entry::Message { message, .. } = entry {
                    Some(message)
                } else {
                    None
                }
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        self.session.abort();
        SessionStore::forget(self.id.as_str());
    }
}

fn descriptor(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: "test".into(),
        input_schema: json!({"type": "object"}),
    }
}
fn inject(text: &str) -> SessionCommand {
    SessionCommand::Inject {
        input: TurnInput::text(text),
    }
}
fn completion(text: &str, calls: &[(&str, &str)]) -> String {
    let calls: Vec<_> = calls.iter().enumerate().map(|(i, (id, name))| json!({
        "index": i, "id": id, "type": "function", "function": {"name": name, "arguments": "{}"}
    })).collect();
    let stop = if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let chunk = json!({
        "id": "test-response", "choices": [{"index": 0, "delta": {"content": text, "tool_calls": calls}, "finish_reason": stop}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 1}
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}
fn conversation(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system" && m["role"] != "developer")
        .cloned()
        .collect()
}
fn assert_user(message: &Value, text: &str) {
    assert_eq!(message["role"], "user");
    assert_eq!(message["content"], json!([{"type": "text", "text": text}]));
}

#[derive(Debug)]
struct BoundaryBuiltin(mpsc::UnboundedSender<SessionCommand>);
impl BuiltinTools for BoundaryBuiltin {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![descriptor("boundary")]
    }
    fn handles(&self, name: &str) -> bool {
        name == "boundary"
    }
    fn run<'a>(&'a self, _: &'a str, _: Value) -> BoxFuture<'a, BuiltinOutcome> {
        Box::pin(async move {
            // No await between queueing the steer and making the last builtin ready.
            self.0.send(inject("builtin steer")).unwrap();
            BuiltinOutcome {
                text: "builtin result".into(),
                is_error: false,
                images: Vec::new(),
                summary: String::new(),
                job_started: None,
            }
        })
    }
    fn take_job_notices(&self) -> Option<mpsc::UnboundedReceiver<JobNotice>> {
        None
    }
    fn running_jobs(&self) -> usize {
        0
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn last_tool_result_then_inject_is_in_the_very_next_query() {
    let mut f = Fixture::new(false).await;
    f.start();
    let first = f.request().await;
    assert_eq!(conversation(&first.body).len(), 1);
    first.respond(completion("", &[("a", "ours")]));
    f.tool_calls(1).await;
    // On the current-thread runtime both commands are queued before the engine can resume.
    f.result("a", "tool result");
    f.inject("steer one");
    f.inject("steer two");
    let second = f.request().await;
    let messages = conversation(&second.body);
    assert_eq!(
        messages.len(),
        5,
        "the second query must already contain both steering inputs"
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "a");
    assert_eq!(messages[2]["content"], "tool result");
    assert_user(&messages[3], "steer one");
    assert_user(&messages[4], "steer two");
    second.respond(completion("done", &[]));
    f.ended(TurnOutcome::Completed, 2).await;
    f.close().await;
    let history = f.messages();
    assert!(matches!(&history[2], Message::ToolResult { call_id, .. } if call_id == "a"));
    assert_eq!(history[3].text(), "steer one");
    assert_eq!(history[4].text(), "steer two");
}

#[tokio::test]
async fn steering_waits_for_the_entire_tool_batch_and_preserves_result_order() {
    let mut f = Fixture::new(false).await;
    f.start();
    f.request()
        .await
        .respond(completion("", &[("a", "ours"), ("b", "ours")]));
    f.tool_calls(2).await;
    // Out-of-order results, with an injection before the batch completes and one after it.
    f.result("b", "second result");
    f.inject("during batch");
    f.result("a", "first result");
    f.inject("after batch");
    let next = f.request().await;
    let messages = conversation(&next.body);
    assert_eq!(messages.len(), 6);
    for (index, id, text) in [(2, "a", "first result"), (3, "b", "second result")] {
        assert_eq!(messages[index]["role"], "tool");
        assert_eq!(messages[index]["tool_call_id"], id);
        assert_eq!(messages[index]["content"], text);
    }
    assert_user(&messages[4], "during batch");
    assert_user(&messages[5], "after batch");
    next.respond(completion("done", &[]));
    f.ended(TurnOutcome::Completed, 2).await;
    f.close().await;
    let history = f.messages();
    assert!(matches!(&history[2], Message::ToolResult { call_id, .. } if call_id == "a"));
    assert!(matches!(&history[3], Message::ToolResult { call_id, .. } if call_id == "b"));
    assert_eq!(history[4].text(), "during batch");
    assert_eq!(history[5].text(), "after batch");
}

#[tokio::test]
async fn builtin_completion_boundary_includes_result_before_steering() {
    let mut f = Fixture::new(true).await;
    f.start();
    f.request()
        .await
        .respond(completion("", &[("a", "boundary")]));
    let next = f.request().await;
    let messages = conversation(&next.body);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["content"], "builtin result");
    assert_user(&messages[3], "builtin steer");
    next.respond(completion("done", &[]));
    f.ended(TurnOutcome::Completed, 2).await;
    f.close().await;
}

#[tokio::test]
async fn mixed_builtin_and_core_batch_keeps_all_results_before_steering() {
    let mut f = Fixture::new(true).await;
    f.start();
    f.request().await.respond(completion(
        "",
        &[("a", "ours"), ("c", "boundary"), ("b", "ours")],
    ));
    f.tool_calls(2).await;
    f.result("b", "second result");
    f.inject("during batch");
    f.result("a", "first result");
    f.inject("after batch");
    let next = f.request().await;
    let messages = conversation(&next.body);
    assert_eq!(messages.len(), 8);
    for (index, id, text) in [
        (2, "a", "first result"),
        (3, "c", "builtin result"),
        (4, "b", "second result"),
    ] {
        assert_eq!(messages[index]["role"], "tool");
        assert_eq!(messages[index]["tool_call_id"], id);
        assert_eq!(messages[index]["content"], text);
    }
    assert_user(&messages[5], "builtin steer");
    assert_user(&messages[6], "during batch");
    assert_user(&messages[7], "after batch");
    next.respond(completion("done", &[]));
    f.ended(TurnOutcome::Completed, 2).await;
    f.close().await;
    let history = f.messages();
    for (index, id) in [(2, "a"), (3, "c"), (4, "b")] {
        assert!(matches!(&history[index], Message::ToolResult { call_id, .. } if call_id == id));
    }
    assert_eq!(history[5].text(), "builtin steer");
}

#[tokio::test]
async fn steering_during_compaction_reaches_the_next_agent_query_not_the_summary() {
    let mut f = Fixture::new(false).await;
    f.start();
    f.request().await.respond(completion("", &[("a", "ours")]));
    f.tool_calls(1).await;
    f.result("a", &"x".repeat(100_000));
    let summary = f.request().await;
    assert!(summary.body.get("tools").is_none());
    assert!(
        summary
            .body
            .to_string()
            .contains("Write a structured checkpoint")
    );
    f.inject("compaction steer");
    summary.respond(completion("summary of start", &[]));
    let next = f.request().await;
    let messages = conversation(&next.body);
    assert_user(messages.last().unwrap(), "compaction steer");
    assert_eq!(messages[messages.len() - 2]["role"], "tool");
    assert!(
        messages[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("summary of start")
    );
    next.respond(completion("done", &[]));
    f.ended(TurnOutcome::Completed, 2).await;
    f.close().await;
    assert_eq!(
        f.events
            .iter()
            .filter(|e| matches!(e, EngineEvent::Compacted { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn credential_lookup_steering_is_applied_on_initial_and_retry_attempts() {
    for retry in [false, true] {
        let mut f = Fixture::new(false).await;
        *f.credentials.on_read.lock().unwrap() = if retry {
            VecDeque::from([vec![], vec![inject("credential steer")]])
        } else {
            VecDeque::from([vec![inject("credential steer")]])
        };
        f.start();
        if retry {
            let first = f.request().await;
            assert_eq!(conversation(&first.body).len(), 1);
            first
                .reply
                .send((503, "{\"error\":{\"message\":\"try again\"}}".into()))
                .unwrap();
        }
        let next = f.request().await;
        let messages = conversation(&next.body);
        assert_eq!(messages.len(), 2);
        assert_user(&messages[1], "credential steer");
        next.respond(completion("done", &[]));
        f.ended(TurnOutcome::Completed, 1).await;
        f.close().await;
        assert_eq!(
            f.events
                .iter()
                .filter(|e| matches!(e, EngineEvent::ModelCallStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            f.events
                .iter()
                .filter(|e| matches!(
                    e,
                    EngineEvent::ModelCallFinished {
                        stop: StopReason::EndTurn,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            f.events
                .iter()
                .filter(|e| matches!(e, EngineEvent::Retrying { .. }))
                .count(),
            usize::from(retry)
        );
    }
}

#[tokio::test]
async fn explicit_cancel_behind_the_last_result_prevents_another_query() {
    for cancel in [
        SessionCommand::Interrupt,
        SessionCommand::Close,
        SessionCommand::Forget,
    ] {
        let mut f = Fixture::new(false).await;
        f.start();
        f.request()
            .await
            .respond(completion("", &[("a", "ours"), ("b", "ours")]));
        f.tool_calls(2).await;
        f.result("b", "second result");
        f.result("a", "first result");
        f.send(cancel.clone());
        f.ended(TurnOutcome::Interrupted, 1).await;
        if cancel == SessionCommand::Interrupt {
            f.close().await;
        } else {
            f.closed().await;
        }
        assert!(
            f.requests.try_recv().is_err(),
            "cancel must not send another query"
        );
        if cancel != SessionCommand::Forget {
            let history = f.messages();
            assert_eq!(history.len(), 4);
            assert!(matches!(&history[2], Message::ToolResult { call_id, .. } if call_id == "a"));
            assert!(matches!(&history[3], Message::ToolResult { call_id, .. } if call_id == "b"));
        }
    }
}

#[tokio::test]
async fn explicit_cancel_during_credentials_prevents_the_request_snapshot() {
    for cancel in [
        SessionCommand::Interrupt,
        SessionCommand::Close,
        SessionCommand::Forget,
    ] {
        let mut f = Fixture::new(false).await;
        *f.credentials.on_read.lock().unwrap() = VecDeque::from([vec![cancel.clone()]]);
        f.start();
        f.ended(TurnOutcome::Interrupted, 1).await;
        if cancel == SessionCommand::Interrupt {
            f.close().await;
        } else {
            f.closed().await;
        }
        assert!(f.requests.try_recv().is_err());
        assert!(
            !f.events
                .iter()
                .any(|e| matches!(e, EngineEvent::ModelCallFinished { .. }))
        );
    }
}

#[tokio::test]
async fn steering_does_not_cancel_an_in_flight_request_or_tool() {
    let mut f = Fixture::new(false).await;
    f.start();
    let first = f.request().await;
    f.inject("while model runs");
    first.respond(completion("", &[("a", "ours"), ("b", "ours")]));
    f.tool_calls(2).await;
    f.inject("while tools run");
    f.result("a", "first result");
    f.result("b", "second result");
    let next = f.request().await;
    let messages = conversation(&next.body);
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[2]["content"], "first result");
    assert_eq!(messages[3]["content"], "second result");
    assert_user(&messages[4], "while model runs");
    assert_user(&messages[5], "while tools run");
    next.respond(completion("done", &[]));
    f.ended(TurnOutcome::Completed, 2).await;
    f.close().await;
}
