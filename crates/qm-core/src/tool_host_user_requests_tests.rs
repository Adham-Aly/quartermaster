//! Full dispatcher → handler → core host → durable source lookup coverage.
use super::*;
use qm_tools::defs::FindUserRequestsArgs;
use qm_tools::handler::{Dispatcher, ToolCx};
use qm_types::clock::SystemClock;
use qm_types::ids::AgentId;
use serde_json::{Value, json};

#[tokio::test]
async fn find_user_requests_is_read_only_for_both_orchestrators_and_own_sources_only() {
    let dir = std::env::temp_dir().join(format!("qm-find-user-requests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("state.db")).unwrap();
    let originals = db.call(|conn| {
        conn.execute_batch("INSERT INTO secondmates (id, name, scope, created_at, seq)
            VALUES (1, 'sm', '', 0, 3);
            INSERT INTO agents (id, role, home, status, engine, created_at, seq, supervisor_id, secondmate_id) VALUES
            (1, 'quartermaster', '/seat', 'idle', 'claude', 0, 1, NULL, NULL),
            (2, 'secondmate', '/sm', 'idle', 'claude', 0, 2, 1, 1);")?;
        qm_store::seq::observe_all(conn)?;
        let mut ids = Vec::new();
        for agent in [AgentId::new(1), AgentId::new(2)] {
            let mut own = Vec::new();
            for n in 0..14 {
                let text = format!("Original {agent:?} #{n}: 100%_premium\\path \"quote\"\nOnly this task.");
                let id = messages::insert(conn, &MessageRow {
                    agent, source: MessageSource::You, kind: MessageKind::Message,
                    text: &text, options: &[], replies_to: None, turn: None,
                    at: jiff::Timestamp::UNIX_EPOCH,
                })?;
                messages::set_text(conn, id, "rewritten chat")?;
                own.push((id, text));
            }
            ids.push(own);
        }
        conn.execute("DELETE FROM messages", [])?;
        Ok(ids)
    }).await.unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let conversation = Arc::new(Conversation::new(
        db.clone(),
        Arc::clone(&clock),
        AgentId::new(1),
    ));
    let host = CoreToolHost::new(db, Arc::clone(&clock), conversation);
    let dispatcher = Dispatcher::builtin();
    for (index, role) in [Role::Quartermaster, Role::Secondmate]
        .into_iter()
        .enumerate()
    {
        let caller = Caller {
            agent: AgentId::new(i64::try_from(index).unwrap() + 1),
            role,
            turn: None,
            task: None,
            secondmate: None,
            plan: None,
        };
        let cx = ToolCx {
            caller: &caller,
            clock: clock.as_ref(),
            host: &host,
        };
        assert_lookup(&cx, &originals, index).await;
    }
    let crewmate = Caller {
        agent: AgentId::new(1),
        role: Role::Crewmate,
        turn: None,
        task: None,
        secondmate: None,
        plan: None,
    };
    assert!(
        host.find_user_requests(&crewmate, FindUserRequestsArgs::default())
            .await
            .is_err()
    );
    let cx = ToolCx {
        caller: &crewmate,
        clock: clock.as_ref(),
        host: &host,
    };
    assert!(
        dispatcher
            .dispatch(&cx, "find_user_requests", json!({}))
            .await
            .result
            .is_error
    );
    drop(host);
    std::fs::remove_dir_all(dir).unwrap();
}

async fn assert_lookup(cx: &ToolCx<'_>, originals: &[Vec<(MessageId, String)>], index: usize) {
    let dispatcher = Dispatcher::builtin();
    let first = dispatcher
        .dispatch(cx, "find_user_requests", json!({"query": "100%_PREMIUM"}))
        .await;
    assert!(!first.result.is_error, "{}", first.result.text);
    assert!(!first.counted, "lookup is never an effective action");
    let first: Value = serde_json::from_str(&first.result.text).unwrap();
    let requests = first["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 12);
    assert_eq!(
        requests[0],
        json!({"message": originals[index][13].0.get(), "text": originals[index][13].1})
    );
    let page = dispatcher
        .dispatch(
            cx,
            "find_user_requests",
            json!({"before": requests[11]["message"]}),
        )
        .await;
    assert!(!page.result.is_error);
    let page: Value = serde_json::from_str(&page.result.text).unwrap();
    assert_eq!(page["requests"].as_array().unwrap().len(), 2);
    assert_eq!(page["requests"][1]["message"], originals[index][0].0.get());
    for cursor in [originals[1 - index][13].0.get(), -1] {
        let empty = dispatcher
            .dispatch(cx, "find_user_requests", json!({"before": cursor}))
            .await;
        assert!(!empty.result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&empty.result.text).unwrap(),
            json!({"requests": []})
        );
    }
    for invalid in [
        json!({"limit": 100}),
        json!({"agent": 2}),
        json!({"before": "not an ID"}),
    ] {
        assert!(
            dispatcher
                .dispatch(cx, "find_user_requests", invalid)
                .await
                .result
                .is_error
        );
    }
}
