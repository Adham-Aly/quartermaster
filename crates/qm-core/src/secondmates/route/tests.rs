use super::*;
use crate::secondmates::SecondmateDeps;
use crate::turns::forget_tests::Fixture;
use qm_store::messages;
use qm_tools::handler::TurnCx;
use qm_types::domain::Role;
use qm_types::ids::{AgentId, MessageId, ModelId, TurnId};
use std::sync::Arc;

fn service(tasks: &crate::tasks::TaskService) -> SecondmateService {
    let d = tasks.deps();
    SecondmateService::new(SecondmateDeps {
        db: d.db.clone(),
        clock: d.clock.clone(),
        config: d.config.clone(),
        data_dir: d.data_dir.clone(),
        engines: d.engines.clone(),
        model: Arc::new(crate::seat::RoutedSeatModel::new(
            d.db.clone(),
            d.clock.clone(),
            d.config.clone(),
            d.health.clone(),
        )),
        decider: Arc::new(crate::secondmates::DeclaredDecider),
        inbox: d.inbox.clone(),
        held: crate::held::HeldService::new(
            d.db.clone(),
            d.clock.clone(),
            Arc::new(crate::held::InboxSignal(d.inbox.clone())),
        ),
        dispatcher: d.dispatcher,
        ui: Arc::new(crate::api::Conversation::new(
            d.db.clone(),
            d.clock.clone(),
            AgentId::new(1),
        )),
        monitor: d.monitor.clone(),
    })
}

fn user_message(
    conn: &rusqlite::Connection,
    agent: AgentId,
) -> Result<MessageId, qm_store::StoreError> {
    user_says(conn, agent, "routing guidance")
}

fn user_says(
    conn: &rusqlite::Connection,
    agent: AgentId,
    text: &str,
) -> Result<MessageId, qm_store::StoreError> {
    messages::insert(
        conn,
        &messages::NewMessage {
            agent,
            source: qm_types::domain::MessageSource::You,
            kind: qm_types::domain::MessageKind::Message,
            text,
            options: &[],
            replies_to: None,
            turn: None,
            at: jiff::Timestamp::UNIX_EPOCH,
        },
    )
}

#[tokio::test]
async fn preference_only_routes_verify_attached_citations_but_allow_none() {
    let f = Fixture::new().await;
    let (tasks, scheduler) = f.task_service();
    let service = service(&tasks);
    let data = tasks.deps().data_dir.clone();
    let (sm, own, foreign) = f
        .db()
        .call(move |conn| {
            let sm = secondmates::insert(
                conn,
                &secondmates::NewSecondmate {
                    name: "routing",
                    scope: "test",
                    projects: &[],
                    standing_branch: None,
                    parent: AgentId::new(1),
                    data_dir: &data,
                    at: jiff::Timestamp::UNIX_EPOCH,
                },
            )?;
            let own = user_message(conn, AgentId::new(1))?;
            let foreign = user_message(conn, sm.agent)?;
            Ok((sm.id, own, foreign))
        })
        .await
        .unwrap();
    let caller = Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: Some(TurnCx {
            turn: TurnId::new(1),
            started_by: StartedBy::AppEvent,
        }),
        task: None,
        secondmate: None,
        plan: None,
    };
    for (citation, valid) in [
        (Some(own), true),
        (Some(foreign), false),
        (Some(MessageId::new(-1)), false),
        (None, true),
    ] {
        let req = RouteRequest {
            secondmate: sm,
            request: "Do the work".into(),
            instructions: vec![],
            base_branch: None,
            model: None,
            tier: None,
            effort: None,
            premium: false,
            preference: Some(qm_types::model_preference::ModelPreference {
                selection: ModelId::new("guidance").unwrap(),
                provider: None,
                model: None,
                effort: None,
            }),
            choice_message: citation,
            later_steps: vec![],
            words_message: None,
            words_cut: vec![],
        };
        let result = service.route(&caller, req).await;
        if valid {
            let routed = result.unwrap();
            f.db()
                .call(move |conn| {
                    let stored = secondmates::load_request(conn, routed.correlation)?.unwrap();
                    assert_eq!(stored.overrides.choice_message, citation);
                    assert!(stored.overrides.preference.is_some());
                    Ok(())
                })
                .await
                .unwrap();
        } else {
            assert!(matches!(result, Err(ToolFailure::Refused(_))), "{result:?}");
        }
    }
    f.db()
        .call(|conn| {
            let count: i64 =
                conn.query_row("SELECT count(*) FROM secondmate_requests", [], |r| r.get(0))?;
            assert_eq!(count, 2, "invalid citations create no request");
            Ok(())
        })
        .await
        .unwrap();
    drop(service);
    drop(tasks);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn a_routed_request_carries_the_chosen_message_less_its_cuts() {
    let f = Fixture::new().await;
    let (tasks, scheduler) = f.task_service();
    let service = service(&tasks);
    let data = tasks.deps().data_dir.clone();
    let (sm, agent, earlier, latest, foreign) = f
        .db()
        .call(move |conn| {
            let sm = secondmates::insert(
                conn,
                &secondmates::NewSecondmate {
                    name: "words",
                    scope: "test",
                    projects: &[],
                    standing_branch: None,
                    parent: AgentId::new(1),
                    data_dir: &data,
                    at: jiff::Timestamp::UNIX_EPOCH,
                },
            )?;
            let earlier = user_says(
                conn,
                AgentId::new(1),
                "Fix the flaky upload test. Once that lands, spin up a crewmate for the docs.",
            )?;
            let latest = user_says(conn, AgentId::new(1), "Send it to the web secondmate.")?;
            let foreign = user_says(conn, sm.agent, "The secondmate's own conversation.")?;
            Ok((sm.id, sm.agent, earlier, latest, foreign))
        })
        .await
        .unwrap();
    let caller = Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: Some(TurnCx {
            turn: TurnId::new(1),
            started_by: StartedBy::User { message: latest },
        }),
        task: None,
        secondmate: None,
        plan: None,
    };
    let route = |words_message, words_cut: Vec<String>| RouteRequest {
        secondmate: sm,
        request: "Fix the flaky upload test".into(),
        instructions: vec![],
        base_branch: None,
        model: None,
        tier: None,
        effort: None,
        premium: false,
        preference: None,
        choice_message: None,
        later_steps: vec![],
        words_message,
        words_cut,
    };
    let refused = service.route(&caller, route(Some(foreign), vec![])).await;
    assert!(
        matches!(refused, Err(ToolFailure::Refused(_))),
        "{refused:?}"
    );

    let cut = "Once that lands, spin up a crewmate for the docs.".to_owned();
    let routed = service
        .route(&caller, route(Some(earlier), vec![cut.clone()]))
        .await
        .unwrap();
    f.db()
        .call(move |conn| {
            let stored = secondmates::load_request(conn, routed.correlation)?.unwrap();
            assert_eq!(stored.user_words, "Fix the flaky upload test.");
            assert_eq!(stored.user_words_cuts, vec![cut]);
            let wake: String = conn.query_row(
                "SELECT payload FROM wakes WHERE target_agent_id = ?1",
                [agent],
                |r| r.get(0),
            )?;
            let wake: serde_json::Value = serde_json::from_str(&wake).unwrap();
            let text = wake["text"].as_str().unwrap();
            assert!(
                text.contains(
                    "The user's words (parts about other work left out by the Quartermaster): \
                     \"Fix the flaky upload test.\""
                ),
                "{text}"
            );
            assert!(!text.contains("docs"), "{text}");
            Ok(())
        })
        .await
        .unwrap();
    drop(service);
    drop(tasks);
    scheduler.abort();
    let _ = scheduler.await;
}
