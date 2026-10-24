//! A review's report under each autonomy: the static page, the wait on the user's go-ahead, its
//! card, and what releases it (the card, `approve_review` on your turn, a switch to auto, a steer).
use super::*;
use crate::actions::ActionsPart;
use crate::tasks::review;
use qm_tools::defs::ApproveReviewArgs;
use qm_tools::handler::TurnCx;
use qm_types::api::{ActionKey, ActionKind, CardAction, CardKind};
use qm_types::domain::{StartedBy, TaskStatus};
use qm_types::ids::{MessageId, TurnId};
use std::sync::Arc;

/// The review's done report: one error and one warning.
fn findings() -> Value {
    json!({
        "outcome": "done",
        "answer": "Two findings in the refresh path.",
        "review": {
            "verdict": "findings",
            "findings": [
                {"severity": "error", "file": "src/auth.rs", "line": 3, "summary": "The refresh races the logout.", "action": "auto_fix"},
                {"severity": "warning", "file": "src/auth.rs", "summary": "The retry has no ceiling.", "action": "ask_user"}
            ],
            "risk": "medium",
            "risk_reason": "it touches sign-in"
        }
    })
}

/// A turn your message started.
fn user() -> StartedBy {
    StartedBy::User {
        message: MessageId::new(1),
    }
}

fn qm(started_by: StartedBy) -> Caller {
    Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        task: None,
        secondmate: None,
        turn: Some(TurnCx {
            turn: TurnId::new(1),
            started_by,
        }),
        plan: None,
    }
}

fn review_task() -> TaskId {
    TaskId::new(3).unwrap()
}

/// The host for the review's tools: its report, and the orchestrator's `approve_review`.
#[derive(Debug)]
struct ReviewHost(TaskService);
impl ToolHost for ReviewHost {
    fn record_report<'a>(
        &'a self,
        caller: &'a Caller,
        report: ReportArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(self.0.record_report(caller, report))
    }
    fn approve_review<'a>(
        &'a self,
        caller: &'a Caller,
        task: TaskId,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(review::approve_review(&self.0, caller, task))
    }
}

impl Delivery {
    fn svc(&self) -> &TaskService {
        &self.host.0
    }

    async fn set_autonomy(&self, autonomy: &'static str) {
        self.fixture
            .db()
            .call(move |conn| {
                conn.execute("UPDATE projects SET autonomy = ?1 WHERE id = 1", [autonomy])?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn approve(&self, caller: &Caller) -> qm_engine::ToolResult {
        let host = ReviewHost(self.svc().clone());
        Dispatcher::builtin()
            .dispatch(
                &ToolCx {
                    caller,
                    clock: self.svc().clock(),
                    host: &host,
                },
                "approve_review",
                serde_json::to_value(ApproveReviewArgs {
                    task: review_task(),
                })
                .unwrap(),
            )
            .await
            .result
    }

    async fn actions(&self) -> Vec<qm_types::api::ActionItem> {
        let svc = self.svc();
        let deps = svc.deps();
        let held =
            crate::held::HeldService::new(deps.db.clone(), deps.clock.clone(), Arc::new(NoSignal));
        let part = ActionsPart::new(
            held,
            (deps.db.clone(), deps.clock.clone(), deps.data_dir.clone()),
            AgentId::new(1),
            svc.clone(),
        );
        let mut snapshot = Snapshot::default();
        part.fill(&mut snapshot).await;
        snapshot.actions
    }

    async fn assert_no_card(&self) {
        assert_eq!(self.actions().await, [] as [qm_types::api::ActionItem; 0]);
    }

    /// The review's status, whether it waits on the go-ahead, and the wakes its supervisor has.
    async fn state(&self) -> (TaskStatus, bool, Vec<(String, String)>) {
        self.fixture
            .db()
            .call(|conn| {
                let row = tasks::load(conn, review_task())?.unwrap();
                let mut stmt = conn.prepare(
                    "SELECT kind, json_extract(payload, '$.text') FROM wakes
                     WHERE target_agent_id = 1 ORDER BY seq",
                )?;
                let wakes = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((row.status, awaits_go_ahead(&row), wakes))
            })
            .await
            .unwrap()
    }

    /// Reports the review done with its page written: it waits on the user (normal autonomy).
    async fn waiting(kind_of_wait: &str) -> Self {
        let delivery = Delivery::new(TaskKind::Review).await;
        delivery.write_page();
        let result = delivery.report(findings()).await;
        assert!(!result.is_error, "{kind_of_wait}: {result:?}");
        assert_eq!(
            delivery.state().await,
            (TaskStatus::Waiting, true, Vec::new())
        );
        delivery
    }
}

#[derive(Debug)]
struct NoSignal;
impl crate::held::WakeSignal for NoSignal {
    fn signal(&self, _: AgentId) {}
}

/// The card of the review waiting on you: what it found, and Open and Go ahead.
fn assert_review_card(item: &qm_types::api::ActionItem) {
    assert_eq!(
        item.key,
        ActionKey::Review {
            task: review_task()
        }
    );
    assert_eq!(item.kind, ActionKind::Review);
    assert_eq!(item.task, Some(review_task()));
    assert_eq!(item.card.kind, CardKind::Review);
    assert_eq!(item.card.heading, REVIEW_HEADING);
    let words: String = item
        .card
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(
        words.contains("Verdict findings · risk medium · 2 findings: 1 error, 1 warning"),
        "{words}"
    );
    assert!(words.contains("the Quartermaster"), "{words}");
    let buttons: Vec<_> = item.card.buttons.iter().map(|b| &b.action).collect();
    assert_eq!(
        buttons,
        [
            &CardAction::OpenReport {
                task: review_task()
            },
            &CardAction::ApproveReview {
                task: review_task()
            }
        ]
    );
}

#[tokio::test]
async fn normal_review_writes_its_report_and_waits_on_the_users_go_ahead() {
    let delivery = Delivery::new(TaskKind::Review).await;
    // The template is seeded, and a done review can't report without its own page.
    assert!(delivery.page().with_file_name("template.html").is_file());
    assert!(delivery.page().with_file_name("SKILL.md").is_file());
    let refused = delivery.report(findings()).await;
    assert!(
        refused.is_error && refused.text.contains("$QM_REPORT_DIR/report.html"),
        "{refused:?}"
    );
    delivery.assert_unreported().await;
    delivery.write_page();
    let accepted = delivery.report(findings()).await;
    assert!(!accepted.is_error, "{accepted:?}");
    assert!(
        accepted.text.contains("The user has your review's report"),
        "{accepted:?}"
    );

    // Waiting on the user, and nothing reaches the supervisor yet.
    assert_eq!(
        delivery.state().await,
        (TaskStatus::Waiting, true, Vec::new())
    );
    let events: i64 = delivery
        .fixture
        .db()
        .call(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM events WHERE kind = ?1",
                [review::TO_USER_EVENT],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(events, 1);

    // The report opens from the task pane, and the card waits in Actions.
    let snapshot = delivery.fill().await;
    assert!(snapshot.tasks[0].report_available);
    let part = ReportPart::new(delivery.svc().clone());
    assert_eq!(part.page(review_task()).await.unwrap(), delivery.page());
    let actions = delivery.actions().await;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_review_card(&actions[0]);

    // An agent can't give the go-ahead on its own turn.
    let refused = delivery.approve(&qm(StartedBy::AppEvent)).await;
    assert!(refused.is_error, "{refused:?}");
    assert_eq!(delivery.state().await.0, TaskStatus::Waiting);

    // The card's Go ahead releases it: reported, and the supervisor gets the report.
    part.handle(&Command::ApproveReview {
        task: review_task(),
    })
    .unwrap()
    .await;
    let (status, waits, wakes) = delivery.state().await;
    assert_eq!((status, waits), (TaskStatus::Reported, false));
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    let (kind, text) = &wakes[0];
    assert_eq!(kind, "reported");
    assert!(text.contains("Two findings in the refresh path."), "{text}");
    assert!(text.contains("pressed Go ahead on its card"), "{text}");
    assert!(
        text.contains("if neither says, do nothing with them"),
        "{text}"
    );
    delivery.assert_no_card().await;
    assert!(delivery.fill().await.tasks[0].report_available);

    // A second go-ahead finds nothing to approve.
    let again = delivery.approve(&qm(user())).await;
    assert!(
        again.is_error && again.text.contains("isn't waiting"),
        "{again:?}"
    );
}

#[tokio::test]
async fn a_go_ahead_given_in_chat_reaches_the_supervisor_in_the_result() {
    let delivery = Delivery::waiting("chat").await;
    let result = delivery.approve(&qm(user())).await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.text.contains("Go-ahead recorded."), "{result:?}");
    // The Quartermaster is the review's supervisor: the report comes in the result, not a wake.
    assert!(
        result.text.contains("Two findings in the refresh path."),
        "{result:?}"
    );
    assert!(result.text.contains("settle_task"), "{result:?}");
    assert_eq!(
        delivery.state().await,
        (TaskStatus::Reported, false, Vec::new())
    );
}

#[tokio::test]
async fn a_waiting_review_refuses_to_settle_until_the_go_ahead() {
    use crate::delivery::{DeliveryDeps, DeliveryService};
    use crate::settling::SettlingService;
    use crate::verification::{VerificationDeps, VerificationService};

    #[derive(Debug)]
    struct NoNetwork;
    impl qm_github::TokenSource for NoNetwork {
        fn token(&self) -> BoxFuture<'_, Result<qm_github::GithubToken, qm_github::GithubError>> {
            Box::pin(async { panic!("a review's settlement must not contact GitHub") })
        }
        fn invalidate(&self) {
            panic!("no GitHub login in this fixture")
        }
    }
    impl crate::delivery::ports::ParentPort for NoNetwork {
        fn task_settled(
            &self,
            _: &rusqlite::Connection,
            _: TaskId,
            _: &crate::settling::Fact,
        ) -> Result<(), qm_store::StoreError> {
            Ok(())
        }
    }

    let delivery = Delivery::waiting("settle").await;
    let deps = delivery.svc().deps();
    let verification = VerificationService::new(VerificationDeps {
        db: deps.db.clone(),
        clock: deps.clock.clone(),
        data_dir: deps.data_dir.clone(),
        health: deps.health.clone(),
        inbox: deps.inbox.clone(),
        chat: deps.chat.clone(),
    });
    let settling = SettlingService::new(DeliveryService::new(DeliveryDeps {
        tasks: delivery.svc().clone(),
        github: qm_github::GithubClient::new(Arc::new(NoNetwork)),
        gh: qm_github::fixed_gh(deps.home.join("no-gh")),
        verification: Arc::new(verification),
        parent: Arc::new(NoNetwork),
    }));
    let ToolFailure::Refused(refusal) = settling
        .settle_task(&qm(StartedBy::AppEvent), review_task())
        .await
        .unwrap_err()
    else {
        panic!("expected a refusal")
    };
    assert!(
        refusal.reason.contains("waits for the user's go-ahead"),
        "{}",
        refusal.reason
    );
    assert_eq!(delivery.state().await.0, TaskStatus::Waiting);
}

#[tokio::test]
async fn auto_review_reports_to_its_supervisor_at_once_without_a_page() {
    let delivery = Delivery::new(TaskKind::Review).await;
    delivery.set_autonomy("auto").await;
    let result = delivery.report(findings()).await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.text.contains("Your supervisor has it"), "{result:?}");
    assert!(!delivery.page().exists());
    let (status, waits, wakes) = delivery.state().await;
    assert_eq!((status, waits), (TaskStatus::Reported, false));
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert!(wakes[0].1.contains("Two findings in the refresh path."));
    assert!(wakes[0].1.contains("don't wait for the user"), "{wakes:?}");
    delivery.assert_no_card().await;
}

#[tokio::test]
async fn switching_to_auto_releases_a_waiting_review() {
    let delivery = Delivery::waiting("auto").await;
    delivery.set_autonomy("auto").await;
    delivery.svc().autonomy_changed("p").await;
    let (status, waits, wakes) = delivery.state().await;
    assert_eq!((status, waits), (TaskStatus::Reported, false));
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert!(
        wakes[0].1.contains("switched to auto autonomy"),
        "{wakes:?}"
    );
    delivery.assert_no_card().await;
}

#[tokio::test]
async fn a_steer_sends_a_waiting_review_back_to_work() {
    let delivery = Delivery::waiting("steer").await;
    let updated = delivery
        .svc()
        .message_task(
            &qm(user()),
            review_task(),
            "Look at the logout path too.".to_owned(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status, TaskStatus::Running);
    let (status, waits, wakes) = delivery.state().await;
    assert_eq!((status, waits), (TaskStatus::Running, false));
    assert_eq!(wakes, []);
    delivery.assert_no_card().await;
}

#[tokio::test]
async fn unfinished_reviews_need_no_page_under_normal_autonomy() {
    for outcome in ["blocked", "needs_decision"] {
        let delivery = Delivery::new(TaskKind::Review).await;
        let result = delivery
            .report(json!({"outcome": outcome, "blockers": ["Need the branch name"]}))
            .await;
        assert!(!result.is_error, "{outcome}: {result:?}");
        assert!(!delivery.page().exists());
        delivery.assert_no_card().await;
    }
}
