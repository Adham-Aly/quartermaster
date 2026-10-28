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

fn secondmate(
    conn: &rusqlite::Connection,
    data: &qm_types::data_dir::DataDir,
    name: &str,
    scope: &str,
) -> Result<secondmates::SecondmateRecord, qm_store::StoreError> {
    secondmates::insert(
        conn,
        &secondmates::NewSecondmate {
            name,
            scope,
            projects: &[],
            standing_branch: None,
            parent: AgentId::new(1),
            data_dir: data,
            at: jiff::Timestamp::UNIX_EPOCH,
        },
    )
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
            let sm = secondmate(conn, &data, "routing", "test")?;
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
async fn a_routed_request_stores_chosen_words_but_delivers_only_curated_text() {
    let f = Fixture::new().await;
    let (tasks, scheduler) = f.task_service();
    let service = service(&tasks);
    let data = tasks.deps().data_dir.clone();
    let (sm, agent, earlier, latest, foreign) = f
        .db()
        .call(move |conn| {
            let sm = secondmate(conn, &data, "words", "test")?;
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
        request: "Stabilize upload coverage".into(),
        instructions: vec!["Run the focused upload tests".into()],
        base_branch: Some("release/upload".into()),
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
            assert_eq!(
                text,
                format!(
                    "Request in your scope (test).\nRequest: Stabilize upload coverage\n\
                     Instructions for this request only:\n- Run the focused upload tests\n\
                     Base branch for this request: release/upload.\n\
                     Reply on the parent channel with corr {}.",
                    routed.correlation
                )
            );
            assert!(!text.contains("Fix the flaky upload test."), "{text}");
            assert!(!text.contains("docs"), "{text}");
            assert_eq!(
                qm_store::user_message_sources::search(conn, AgentId::new(1), None, None)?
                    .iter()
                    .find(|source| source.id == earlier)
                    .unwrap()
                    .text,
                "Fix the flaky upload test. Once that lands, spin up a crewmate for the docs."
            );
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
async fn default_routes_and_charter_changes_do_not_copy_the_triggering_user_message() {
    const ORIGINAL: &str = "Handle uploads. After that, start unrelated billing work.";
    let f = Fixture::new().await;
    let (tasks, scheduler) = f.task_service();
    let service = service(&tasks);
    let data = tasks.deps().data_dir.clone();
    let (sm, message) = f
        .db()
        .call(move |conn| {
            let sm = secondmate(conn, &data, "uploads", "Upload reliability")?;
            let message = user_says(conn, AgentId::new(1), ORIGINAL)?;
            Ok((sm, message))
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
    let routed = service
        .route(
            &caller,
            RouteRequest {
                secondmate: sm.id,
                request: "Stabilize upload retries".into(),
                instructions: vec![],
                base_branch: None,
                model: None,
                tier: None,
                effort: None,
                premium: false,
                preference: None,
                choice_message: None,
                later_steps: vec![],
                words_message: None,
                words_cut: vec![],
            },
        )
        .await
        .unwrap();
    service
        .configure(
            &caller,
            qm_tools::handler::CharterChange {
                secondmate: sm.id,
                scope: Some("Upload testing and reliability".into()),
                standing_branch: Some(Some("release/uploads".into())),
            },
        )
        .await
        .unwrap();
    f.db().call(move |conn| {
        let stored = secondmates::load_request(conn, routed.correlation)?.unwrap();
        assert_eq!(stored.user_words, ORIGINAL);
        assert_eq!(stored.overrides.choice_message, Some(message));
        let after = secondmates::load(conn, sm.id)?.unwrap();
        let charter = super::super::charter::charter_text(&after);
        assert!(charter.contains("Upload testing and reliability"));
        assert!(charter.contains("release/uploads"));
        assert!(!charter.contains(ORIGINAL));
        assert!(!charter.contains("billing"));
        let mut stmt = conn.prepare("SELECT kind, payload FROM wakes WHERE target_agent_id = ?1 ORDER BY seq")?;
        let wakes = stmt.query_map([sm.agent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(wakes.len(), 2);
        for (kind, payload) in wakes {
            let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let text = payload["text"].as_str().unwrap();
            assert!(!text.contains(ORIGINAL), "{text}");
            assert!(!text.contains("billing"), "{text}");
            if kind == "charter_changed" {
                assert_eq!(text, format!("Your charter changed. {charter}"));
            } else {
                assert_eq!(kind, "routed");
                assert_eq!(text, format!(
                    "Request in your scope (Upload reliability).\nRequest: Stabilize upload retries\n\
                     Reply on the parent channel with corr {}.", routed.correlation
                ));
            }
        }
        let sources = qm_store::user_message_sources::search(conn, caller.agent, None, None)?;
        assert_eq!(sources.iter().find(|s| s.id == message).unwrap().text, ORIGINAL);
        Ok(())
    }).await.unwrap();
    drop(service);
    drop(tasks);
    scheduler.abort();
    let _ = scheduler.await;
}
