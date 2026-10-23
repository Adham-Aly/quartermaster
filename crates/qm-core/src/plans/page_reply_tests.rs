//! A plan reply sent from the page, end to end through the core: the checks, the one claim per
//! round shared with the paste path, delivery identical to a paste, recovery after a restart, and
//! the endpoint's HTTP surface.

use super::submit::{self, Status};
use super::*;
use crate::tasks::TaskService;
use crate::turns::forget_tests::Fixture;
use qm_store::events;
use qm_tools::handler::{Caller, TurnCx};
use qm_types::domain::{Role, StartedBy};
use qm_types::ids::{AgentId, MessageId, TaskId, TurnId};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const PLAN_MD: &str = "# Cache plan

## Data flow

The cache sits in front of the queue.

```question
id = \"store\"
ask = \"Which store?\"
choices = [
 { id = \"sqlite\", label = \"SQLite\", recommended = true },
 { id = \"sled\", label = \"sled\" },
]
```

## Assumptions

- None yet.
";

/// A plan task (T-0003) waiting on the user's reply to round 1, with its page rendered to send to
/// `port`, beside the fixture's Quartermaster (agent 1) and the planner (agent 3).
struct Page {
    fixture: Fixture,
    svc: TaskService,
    _scheduler: tokio::task::JoinHandle<()>,
    task: TaskId,
    dir: std::path::PathBuf,
}

const PORT: u16 = 41_049;

impl Page {
    async fn new() -> Self {
        let fixture = Fixture::new().await;
        fixture
            .db()
            .call(|conn| {
                conn.execute_batch(
                    "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode, branch, status, waiting_on, created_at, seq)
                     VALUES (3, 1, 1, 'plan', 'plan(cache): store', 'plan it', 'direct_pr', 'qm/task-3', 'waiting', '{\"plan\":\"reply\"}', 1, 3);
                     INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                     VALUES (3, 'crewmate', 1, 3, '/c3', 'idle', 1, 3);",
                )?;
                qm_store::seq::observe_all(conn)?;
                Ok(())
            })
            .await
            .unwrap();
        let (svc, scheduler) = fixture.task_service();
        fs::create_dir_all(svc.deps().data_dir.artifacts_dir()).unwrap();
        let task = TaskId::new(3).unwrap();
        let dir = svc.plan_dir(task);
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        create(&dir).unwrap();
        fs::write(dir.join(PLAN_FILE), PLAN_MD).unwrap();
        update(&dir, |s| {
            s.stage = Stage::WithUser;
            s.round = 1;
            s.open = vec!["store".into()];
            s.page_token = PageToken::fresh();
        })
        .unwrap();
        let state = load(&dir).unwrap().unwrap();
        let rendered = render(&dir, task, "p", &state, BTreeMap::new(), Some(PORT)).unwrap();
        update(&dir, |s| rendered.record(s)).unwrap();
        svc.set_plan_port(Some(PORT));
        Self {
            fixture,
            svc,
            _scheduler: scheduler,
            task,
            dir,
        }
    }

    fn state(&self) -> PlanState {
        load(&self.dir).unwrap().unwrap()
    }

    fn page(&self) -> String {
        fs::read_to_string(self.dir.join(PAGE_FILE)).unwrap()
    }

    /// The reply block the page's `buildReply` makes for round `round`.
    fn reply(&self, round: u32) -> String {
        format!(
            "<plan-reply task=\"{}\" plan=\"{}\" round=\"{round}\">\n\
             Reply to round {round} of the plan for {} (1 of 1 questions answered).\n\n\
             ## Q1 store\nChoice: sled (sled)\nNote:\n> keep WAL off for now\n</plan-reply>\n",
            self.task,
            self.state().uid,
            self.task
        )
    }

    /// The body the page posts, for this round, with its own key.
    fn body(&self) -> serde_json::Value {
        let state = self.state();
        serde_json::json!({
            "v": 1,
            "task": self.task.to_string(),
            "plan": state.uid,
            "round": state.round,
            "token": state.page_token.as_ref().unwrap().reveal(),
            "reply": self.reply(state.round),
        })
    }

    async fn send(&self, body: &serde_json::Value) -> submit::Answer {
        submit::submit(&self.svc, body.to_string().as_bytes()).await
    }

    /// The planner's messages, the steer wakes for it, and the Quartermaster's one-line notes.
    async fn delivered(&self) -> (Vec<String>, i64, Vec<String>) {
        self.fixture
            .db()
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT text FROM messages WHERE agent_id = 3 AND source = 'supervisor' ORDER BY seq",
                )?;
                let planner = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<Vec<String>, _>>()?;
                let wakes: i64 = conn.query_row(
                    "SELECT count(*) FROM wakes WHERE target_agent_id = 3 AND kind = 'steer'",
                    [],
                    |r| r.get(0),
                )?;
                let mut stmt = conn
                    .prepare("SELECT id, text FROM messages WHERE agent_id = 1 AND source = 'app'")?;
                let notes = stmt
                    .query_map([], |r| Ok((MessageId::new(r.get(0)?), r.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                let ids: Vec<MessageId> = notes.iter().map(|n| n.0).collect();
                let flagged = events::one_line(conn, &ids)?;
                let notes = notes
                    .into_iter()
                    .filter(|n| flagged.contains(&n.0))
                    .map(|n| n.1)
                    .collect();
                Ok((planner, wakes, notes))
            })
            .await
            .unwrap()
    }

    async fn status(&self) -> String {
        self.fixture
            .db()
            .call(|conn| {
                Ok(conn.query_row("SELECT status FROM tasks WHERE id = 3", [], |r| r.get(0))?)
            })
            .await
            .unwrap()
    }

    /// The Quartermaster passing `text` to the planner on the turn the user's message `text`
    /// started: the paste path.
    async fn paste(&self, text: &str) -> Result<(), String> {
        let words = text.to_owned();
        let message = self
            .fixture
            .db()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO messages (id, agent_id, source, kind, text, created_at, seq)
                     VALUES (50, 1, 'you', 'message', ?1, 5, 50)",
                    [&words],
                )?;
                qm_store::seq::observe_all(conn)?;
                Ok(MessageId::new(50))
            })
            .await
            .unwrap();
        let caller = Caller {
            agent: AgentId::new(1),
            role: Role::Quartermaster,
            turn: Some(TurnCx {
                turn: TurnId::new(1),
                started_by: StartedBy::User { message },
            }),
            task: None,
            secondmate: None,
            plan: None,
        };
        self.svc
            .message_task(&caller, self.task, text.to_owned())
            .await
            .map(drop)
            .map_err(|e| e.render())
    }
}

#[tokio::test]
async fn a_page_reply_reaches_the_planner_once_as_a_paste_does() {
    let page = Page::new().await;
    assert!(page.page().contains("id=\"send\""));
    let reply = page.reply(1);
    let sent = page.send(&page.body()).await;
    assert_eq!(sent.status, Status::Sent, "{}", sent.message);
    assert!(sent.message.starts_with("Sent to Quartermaster at "));

    let (planner, wakes, notes) = page.delivered().await;
    // The planner gets exactly what the paste path would have sent it.
    assert_eq!(planner, [qm_policy::markers::neutralise(reply.trim())]);
    assert_eq!(wakes, 1);
    assert_eq!(page.status().await, "running");
    assert_eq!(
        notes,
        ["Plan reply sent from the page · T-0003 round 1 · 1 of 1 answered"]
    );
    let state = page.state();
    assert!(state.pending_reply && state.sent_from_page());
    assert_eq!((state.last_answered_round, state.user_rounds), (1, 1));
    assert_eq!(state.page_claim, None);
    // The page now says where the reply went, and can't send again.
    let html = page.page();
    assert!(html.contains("went to the planner at"));
    assert!(!html.contains("id=\"send\""));
    assert!(html.contains("connect-src &#39;none&#39;"));

    let again = page.send(&page.body()).await;
    assert_eq!(again.status, Status::Duplicate);
    assert_eq!(again.message, "Round 1 already has your reply.");
    let (planner, wakes, _) = page.delivered().await;
    assert_eq!((planner.len(), wakes), (1, 1));
}

#[tokio::test]
async fn the_page_and_paste_paths_deliver_the_same_text() {
    let by_page = Page::new().await;
    assert_eq!(by_page.send(&by_page.body()).await.status, Status::Sent);
    let by_paste = Page::new().await;
    by_paste.paste(&by_paste.reply(1)).await.unwrap();
    let (page_text, ..) = by_page.delivered().await;
    let (paste_text, paste_wakes, paste_notes) = by_paste.delivered().await;
    // Each fixture's plan has its own uid; past that, the planner reads the same words.
    let uid = |p: &Page| p.state().uid;
    assert_eq!(
        page_text[0].replace(&uid(&by_page), "UID"),
        paste_text[0].replace(&uid(&by_paste), "UID")
    );
    assert_eq!(paste_wakes, 1);
    assert_eq!(paste_notes, Vec::<String>::new());
    let (a, b) = (by_page.state(), by_paste.state());
    assert_eq!(
        (a.pending_reply, a.last_answered_round, a.user_rounds),
        (b.pending_reply, b.last_answered_round, b.user_rounds)
    );
}

#[tokio::test]
async fn two_sends_at_once_deliver_one_reply() {
    let page = Page::new().await;
    let body = page.body();
    let (a, b) = tokio::join!(page.send(&body), page.send(&body));
    let mut statuses = [a.status, b.status];
    statuses.sort_by_key(|s| s.http());
    assert_eq!(statuses, [Status::Sent, Status::Duplicate]);
    let (planner, wakes, notes) = page.delivered().await;
    assert_eq!((planner.len(), wakes, notes.len()), (1, 1, 1));
}

#[tokio::test]
async fn a_round_takes_one_reply_across_the_page_and_a_paste() {
    // Page first: the paste is refused, and nothing more reaches the planner.
    let page = Page::new().await;
    assert_eq!(page.send(&page.body()).await.status, Status::Sent);
    let refused = page.paste(&page.reply(1)).await.unwrap_err();
    assert!(
        refused.contains("Round 1's reply was already sent from the plan page"),
        "{refused}"
    );
    assert_eq!(page.delivered().await.0.len(), 1);

    // Paste first: the page's send is a duplicate.
    let page = Page::new().await;
    page.paste(&page.reply(1)).await.unwrap();
    let late = page.send(&page.body()).await;
    assert_eq!(late.status, Status::Duplicate);
    assert_eq!(page.delivered().await.0.len(), 1);
}

#[tokio::test]
async fn stale_forged_and_malformed_sends_deliver_nothing() {
    let page = Page::new().await;
    let with = |key: &str, value: serde_json::Value| {
        let mut body = page.body();
        body[key] = value;
        body
    };
    let wrong_key = with("token", "0".repeat(64).into());
    let other_plan = with("plan", "0123456789abcdef01234567".into());
    let old_round = with("round", 0.into());
    let other_reply = with("reply", page.reply(2).into());
    let more_than_a_reply = with("reply", format!("also: {}", page.reply(1)).into());
    let mut unknown = page.body();
    unknown["extra"] = true.into();
    let cases = [
        (wrong_key, Status::Forbidden),
        (other_plan, Status::Stale),
        (old_round, Status::Stale),
        (other_reply, Status::Invalid),
        (more_than_a_reply, Status::Invalid),
        (unknown, Status::Invalid),
        (with("v", 2.into()), Status::Invalid),
        (with("task", "T-0099".into()), Status::Closed),
    ];
    for (body, want) in cases {
        let answer = page.send(&body).await;
        assert_eq!(answer.status, want, "{body}: {}", answer.message);
    }
    let answer = submit::submit(&page.svc, b"not json").await;
    assert_eq!(answer.status, Status::Invalid);
    assert_eq!(page.delivered().await.0.len(), 0);
    assert_eq!(page.state().page_claim, None);

    // Under auto autonomy the plan decides for itself: the page's reply is refused.
    page.fixture
        .db()
        .call(|conn| {
            conn.execute("UPDATE projects SET autonomy = 'auto' WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(page.send(&page.body()).await.status, Status::Closed);
    assert_eq!(page.delivered().await.0.len(), 0);
}

#[tokio::test]
async fn a_restart_settles_a_claimed_reply_by_whether_it_was_delivered() {
    // Claimed, never delivered: the claim goes and the page can send.
    let page = Page::new().await;
    update(&page.dir, |s| s.page_claim = Some(1)).unwrap();
    assert_eq!(page.send(&page.body()).await.status, Status::Duplicate);
    submit::recover(&page.svc).await;
    assert_eq!(page.state().page_claim, None);
    assert_eq!(page.send(&page.body()).await.status, Status::Sent);

    // Delivered, not finished: the round is recorded as sent and the page shows it.
    let page = Page::new().await;
    assert_eq!(page.send(&page.body()).await.status, Status::Sent);
    update(&page.dir, |s| {
        s.page_claim = Some(1);
        s.page_sent = None;
        s.pending_reply = false;
        s.last_answered_round = 0;
        s.user_rounds = 0;
    })
    .unwrap();
    submit::recover(&page.svc).await;
    let state = page.state();
    assert_eq!(state.page_claim, None);
    assert!(state.sent_from_page() && state.pending_reply);
    assert_eq!(state.user_rounds, 1);
    assert!(page.page().contains("went to the planner at"));
    assert_eq!(page.delivered().await.0.len(), 1);
}

#[tokio::test]
async fn the_card_and_approval_follow_a_reply_from_the_page() {
    let page = Page::new().await;
    let words = |p: &Page| {
        let card = api::card(p.task, "plan(cache): store", "p", &p.state(), &p.dir);
        format!("{:?}", card.lines)
    };
    assert!(words(&page).contains("then Send to Quartermaster (or Copy reply and paste it here)."));
    update(&page.dir, |s| {
        s.stage = Stage::Final;
        s.open.clear();
        s.page_claim = Some(1);
    })
    .unwrap();
    let caller = Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: None,
        task: None,
        secondmate: None,
        plan: None,
    };
    let refused = crate::tasks::plan::approve_plan(&page.svc, &caller, page.task, false)
        .await
        .unwrap_err()
        .render();
    assert!(
        refused.contains("approve when the next round is back"),
        "{refused}"
    );
    assert_ne!(page.state().stage, Stage::Approved);
    update(&page.dir, |s| s.page_claim = None).unwrap();
    assert_eq!(page.send(&page.body()).await.status, Status::Sent);
    assert!(
        words(&page).contains("Your reply to round 1 went to the planner; it's writing round 2.")
    );
}

#[tokio::test]
async fn an_approved_plan_drops_its_key_and_its_page_is_closed() {
    let page = Page::new().await;
    update(&page.dir, |s| {
        s.stage = Stage::Final;
        s.open.clear();
    })
    .unwrap();
    // The final page, as it was before the approval.
    let late = page.body();
    let caller = Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: None,
        task: None,
        secondmate: None,
        plan: None,
    };
    crate::tasks::plan::approve_plan(&page.svc, &caller, page.task, false)
        .await
        .unwrap();
    let state = page.state();
    assert_eq!(state.stage, Stage::Approved);
    assert!(state.page_token.is_none());
    let answer = page.send(&late).await;
    assert_eq!(answer.status, Status::Closed);
    assert_eq!(page.delivered().await.0.len(), 0);
}

/// One raw request to the endpoint on `port`, and the whole response.
async fn http(port: u16, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    String::from_utf8(out).unwrap()
}

fn post(port: u16, origin: &str, body: &str) -> String {
    format!(
        "POST /v1/plan-reply HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\n\
         Content-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn the_endpoint_answers_only_a_page_on_its_one_route() {
    let page = Page::new().await;
    let endpoint = endpoint::Endpoint::start(page.svc.clone()).await.unwrap();
    let port = endpoint.port();
    let host = format!("127.0.0.1:{port}");
    let status = |r: &str| r.lines().next().unwrap_or_default().to_owned();
    let cases = [
        (format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"), "404"),
        (
            format!("GET /v1/tasks HTTP/1.1\r\nHost: {host}\r\n\r\n"),
            "404",
        ),
        (
            format!("GET /v1/plan-reply HTTP/1.1\r\nHost: {host}\r\n\r\n"),
            "405",
        ),
        (
            post(port, "null", "{}").replace(&host, "evil.example"),
            "403",
        ),
        (post(port, "https://evil.example", "{}"), "403"),
        (
            post(port, "null", "{}").replace("text/plain;charset=UTF-8", "application/json"),
            "415",
        ),
        (
            format!(
                "POST /v1/plan-reply HTTP/1.1\r\nHost: {host}\r\nOrigin: null\r\nContent-Type: text/plain\r\n\r\n"
            ),
            "411",
        ),
        (
            format!(
                "POST /v1/plan-reply HTTP/1.1\r\nHost: {host}\r\nOrigin: null\r\nContent-Type: text/plain\r\nContent-Length: 999999\r\n\r\n"
            ),
            "413",
        ),
        (
            format!(
                "POST /v1/plan-reply HTTP/1.1\r\nHost: {host}\r\nOrigin: null\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
            ),
            "400",
        ),
        (
            format!(
                "POST /v1/plan-reply HTTP/1.1\r\nHost: {host}\r\nHost: {host}\r\nOrigin: null\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}"
            ),
            "400",
        ),
        (post(port, "null", "{}"), "400"),
    ];
    for (request, want) in cases {
        let response = http(port, &request).await;
        assert!(
            status(&response).contains(want),
            "{request:?} → {}",
            status(&response)
        );
        assert!(response.contains("Connection: close"));
        // Nothing is readable cross-origin but the page's own answers.
        if want != "400" && want != "415" {
            assert!(
                !response.contains("Access-Control-Allow-Origin"),
                "{request:?}"
            );
        }
    }
    let preflight = http(
        port,
        &format!("OPTIONS /v1/plan-reply HTTP/1.1\r\nHost: {host}\r\nOrigin: null\r\nAccess-Control-Request-Method: POST\r\n\r\n"),
    )
    .await;
    assert!(status(&preflight).contains("204"));
    assert!(preflight.contains("Access-Control-Allow-Origin: null"));
    assert_eq!(page.delivered().await.0.len(), 0);

    let sent = http(port, &post(port, "null", &page.body().to_string())).await;
    assert!(status(&sent).contains("200"), "{sent}");
    assert!(sent.contains("Access-Control-Allow-Origin: null"));
    assert!(sent.contains("\"status\":\"sent\""));
    let again = http(port, &post(port, "null", &page.body().to_string())).await;
    assert!(status(&again).contains("409"));
    assert_eq!(page.delivered().await.0.len(), 1);
    endpoint.stop(&page.svc).await;
    assert_eq!(page.svc.plan_port(), None);
}

#[tokio::test]
async fn the_endpoint_keeps_its_port_and_moves_open_pages_when_it_cannot() {
    let page = Page::new().await;
    let saved = page.svc.deps().data_dir.plan_endpoint();
    // The saved port is taken: a new one is saved and the open page now sends there.
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let busy = taken.local_addr().unwrap().port();
    fs::write(&saved, format!("{{\"port\":{busy}}}")).unwrap();
    let endpoint = endpoint::Endpoint::start(page.svc.clone()).await.unwrap();
    let port = endpoint.port();
    assert_ne!(port, busy);
    assert_eq!(
        fs::read_to_string(&saved).unwrap(),
        format!("{{\"port\":{port}}}")
    );
    assert_eq!(page.state().page_port, Some(port));
    assert!(
        page.page()
            .contains(&format!("connect-src http://127.0.0.1:{port}"))
    );
    endpoint.stop(&page.svc).await;

    // The next start takes the saved port again, and leaves the page as it is.
    let before = page.page();
    let endpoint = endpoint::Endpoint::start(page.svc.clone()).await.unwrap();
    assert_eq!(endpoint.port(), port);
    assert_eq!(page.page(), before);
    endpoint.stop(&page.svc).await;

    // A plan changed since its page was shown isn't rendered again behind the planner's back.
    fs::write(page.dir.join(PLAN_FILE), format!("{PLAN_MD}\nMore.\n")).unwrap();
    fs::write(&saved, format!("{{\"port\":{busy}}}")).unwrap();
    let endpoint = endpoint::Endpoint::start(page.svc.clone()).await.unwrap();
    assert_ne!(endpoint.port(), busy);
    assert_eq!(page.page(), before);
    endpoint.stop(&page.svc).await;
    drop(taken);
}

/// Collects what the endpoint logs.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_log_never_holds_the_page_key_or_the_reply() {
    // Process-wide, so no test thread's cached "nobody listens" can hide an event from it; it
    // hears every test's log, which only makes the check stricter.
    static CAPTURED: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
    let captured = CAPTURED.get_or_init(|| {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        captured
    });

    let page = Page::new().await;
    let endpoint = endpoint::Endpoint::start(page.svc.clone()).await.unwrap();
    let port = endpoint.port();
    let body = page.body();
    let mut forged = body.clone();
    forged["token"] = "f".repeat(64).into();
    for sent in [&forged, &body, &body] {
        http(port, &post(port, "null", &sent.to_string())).await;
    }
    endpoint.stop(&page.svc).await;

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("plan reply from the page"), "{log}");
    let key = body["token"].as_str().unwrap();
    for secret in [key, &"f".repeat(64), "keep WAL off", "plan-reply task="] {
        assert!(!log.contains(secret), "the log holds {secret:?}");
    }
}
