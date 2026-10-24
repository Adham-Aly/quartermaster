//! Deferred words outlive their optional chat link, independently of choice authority.
use super::*;
use qm_store::{history, messages, tasks, user_message_sources};
use qm_types::domain::{Effort, MessageKind, MessageSource};

const WORDS: &str = "Keep these exact scheduling words in the child's brief.";

async fn schedule_from_user(
    svc: &TaskService,
    with_choice: bool,
) -> (i64, MessageId, Option<MessageId>) {
    let (message, choice) = svc
        .db()
        .call(move |conn| {
            let insert = |text| {
                messages::insert(
                    conn,
                    &messages::NewMessage {
                        agent: caller().agent,
                        source: MessageSource::You,
                        kind: MessageKind::Message,
                        text,
                        options: &[],
                        replies_to: None,
                        turn: None,
                        at: Timestamp::UNIX_EPOCH,
                    },
                )
            };
            // Cite an earlier choice, not the scheduling turn's message.
            let choice = with_choice
                .then(|| insert("Use the high tier and prefer frozen-model with high effort."))
                .transpose()?;
            let message = insert(WORDS)?;
            conn.execute("UPDATE turns SET message_id = ?1 WHERE id = 1", [message])?;
            Ok((message, choice))
        })
        .await
        .unwrap();
    let mut user = caller();
    user.turn = Some(TurnCx {
        turn: TurnId::new(1),
        started_by: StartedBy::User { message },
    });
    let mut req = request();
    req.effort = with_choice.then_some(Effort::High);
    req.tier = with_choice.then_some(qm_types::domain::Tier::High);
    req.preference = with_choice.then(preference);
    req.choice_message = choice;
    let schedule = svc
        .schedule_crewmate(
            &user,
            req,
            StartTrigger::At {
                due: Timestamp::UNIX_EPOCH,
            },
        )
        .await
        .unwrap()
        .schedule;
    svc.db()
        .call(move |conn| {
            let row = deferred::get(conn, schedule)?.unwrap();
            let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
            assert_eq!(row.status, "pending");
            assert!(row.task.is_none());
            assert_eq!(saved.words, WORDS);
            assert_eq!(saved.words_message, Some(message));
            assert_eq!(saved.request.choice_message, choice);
            // A later edit must not replace the task's stored snapshot.
            conn.execute(
                "UPDATE messages SET text = 'edited after registration' WHERE id = ?1",
                [message],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (schedule, message, choice)
}

async fn assert_child(
    svc: &TaskService,
    schedule: i64,
    message: MessageId,
    choice: Option<MessageId>,
    retained: bool,
) {
    svc.db()
        .call(move |conn| {
            let row = deferred::get(conn, schedule)?.unwrap();
            assert_eq!(row.status, "started", "{row:?}");
            let child = tasks::load(conn, row.task.unwrap())?.unwrap();
            assert_eq!(child.user_words, WORDS);
            assert_eq!(child.user_words_message, retained.then_some(message));
            if !retained {
                let brief = qm_store::crew_view::brief_inputs(conn, child.id)?.unwrap();
                assert_eq!(brief.user_words, WORDS, "the brief uses the frozen fallback");
            }
            let (_, event) = tasks::last_event(conn, child.id, start::STARTED_EVENT)?.unwrap();
            assert_eq!(event["route"]["authority"], "user");
            assert_eq!(event["route"]["message"], serde_json::json!(message));
            assert_eq!(event["route"]["choice_message"], serde_json::json!(choice));
            assert_eq!(
                event["route"]["named_effort"],
                serde_json::json!(choice.map(|_| Effort::High))
            );
            if choice.is_some() {
                assert_eq!(event["route"]["named_tier"], "high");
                assert_eq!(event["route"]["preference"], serde_json::json!(preference()));
                let tier: String = conn.query_row(
                    "SELECT tier_landed FROM routing_decisions WHERE task_id = ?1 ORDER BY seq DESC LIMIT 1",
                    [child.id],
                    |r| r.get(0),
                )?;
                assert_eq!(tier, "high");
            }
            let children: i64 = conn.query_row(
                "SELECT count(*) FROM tasks WHERE title = ?1",
                [request().title],
                |r| r.get(0),
            )?;
            assert_eq!(children, 1, "repeated pulses must not allocate another child");
            let events: i64 = conn.query_row(
                "SELECT count(*) FROM events WHERE task_id = ?1 AND kind = ?2",
                rusqlite::params![child.id, start::STARTED_EVENT],
                |r| r.get(0),
            )?;
            assert_eq!(events, 1);
            let violations: i64 =
                conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(violations, 0);
            Ok(())
        })
        .await
        .unwrap();
}

async fn fire_after_reopen(prune: bool, with_choice: bool) {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    // Keep this chat on disk unless it is explicitly pruned, so the control really has a link.
    svc.db()
        .call(|conn| {
            conn.execute(
                "UPDATE agents SET engine = 'provider', chat_kept_from = 1 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (schedule, message, choice) = schedule_from_user(&svc, with_choice).await;
    if prune {
        svc.db()
            .call(|conn| {
                assert_eq!(
                    history::forget_conversation(conn, caller().agent, i64::MAX)?,
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
    }
    let restarted = reopened_service(&svc);
    restarted
        .db()
        .call(move |conn| {
            assert_eq!(messages::get(conn, message)?.is_some(), !prune);
            let turns: i64 =
                conn.query_row("SELECT count(*) FROM turns WHERE id = 1", [], |r| r.get(0))?;
            assert_eq!(turns, i64::from(!prune));
            assert!(user_message_sources::verify(conn, caller().agent, message)?.is_ok());
            if let Some(choice) = choice {
                assert_ne!(choice, message);
                assert_eq!(messages::get(conn, choice)?.is_some(), !prune);
                assert!(user_message_sources::verify(conn, caller().agent, choice)?.is_ok());
            }
            Ok(())
        })
        .await
        .unwrap();
    pulse(&restarted).await.unwrap();
    assert_child(&restarted, schedule, message, choice, !prune).await;
    pulse(&restarted).await.unwrap();
    assert_child(&restarted, schedule, message, choice, !prune).await;
    restarted.shutdown().await;
    svc.shutdown().await;
    drop(restarted);
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn pruned_scheduling_message_fires_after_reopen_without_model_choices() {
    fire_after_reopen(true, false).await;
}

#[tokio::test]
async fn pruned_scheduling_message_keeps_separate_choice_authority_after_reopen() {
    fire_after_reopen(true, true).await;
}

#[tokio::test]
async fn retained_scheduling_message_keeps_its_optional_link_after_reopen() {
    fire_after_reopen(false, false).await;
}
