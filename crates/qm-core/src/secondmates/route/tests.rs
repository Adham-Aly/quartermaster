use super::*;
use crate::secondmates::SecondmateDeps;
use crate::turns::forget_tests::Fixture;
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
    messages::insert(
        conn,
        &messages::NewMessage {
            agent,
            source: qm_types::domain::MessageSource::You,
            kind: qm_types::domain::MessageKind::Message,
            text: "routing guidance",
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
