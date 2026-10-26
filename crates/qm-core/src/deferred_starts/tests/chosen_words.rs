//! A deferred start freezes the words its scheduler chose, trimmed, when it is scheduled.
use super::*;
use qm_store::{messages, tasks};
use qm_types::domain::{MessageKind, MessageSource};

const REQUEST: &str = "Count the lines in notes.txt. After C-0001 finishes, start a review of \
                       project foobar.";

#[tokio::test]
async fn a_deferred_start_carries_the_chosen_message_less_its_cuts() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let (earlier, latest) = svc
        .db()
        .call(|conn| {
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
            Ok((insert(REQUEST)?, insert("Thanks, do that when you can.")?))
        })
        .await
        .unwrap();
    let mut user = caller();
    user.turn = Some(TurnCx {
        turn: TurnId::new(1),
        started_by: StartedBy::User { message: latest },
    });
    let mut req = request();
    req.words_message = Some(earlier);
    req.words_cut = vec!["After C-0001 finishes, start a review of project foobar.".into()];
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
            let saved: Saved = decode(
                "deferred_starts.payload",
                &deferred::get(conn, schedule)?.unwrap().payload,
            )?;
            assert_eq!(saved.words, "Count the lines in notes.txt.");
            assert_eq!(saved.words_message, Some(earlier));
            assert_eq!(saved.words_cuts.len(), 1);
            assert_eq!(saved.request.words_message, None, "frozen, not replayed");
            assert_eq!(saved.request.words_cut, Vec::<String>::new());
            Ok(())
        })
        .await
        .unwrap();

    // A foreign message can't be chosen for a deferred start either.
    let mut foreign = request();
    foreign.words_message = Some(MessageId::new(-1));
    assert!(matches!(
        svc.schedule_crewmate(&user, foreign, future()).await,
        Err(ToolFailure::Refused(_))
    ));

    pulse(&svc).await.unwrap();
    svc.db()
        .call(move |conn| {
            let row = deferred::get(conn, schedule)?.unwrap();
            assert_eq!(row.status, "started", "{row:?}");
            let child = tasks::load(conn, row.task.unwrap())?.unwrap();
            assert_eq!(child.user_words, "Count the lines in notes.txt.");
            assert_eq!(child.user_words_message, Some(earlier));
            let brief = qm_store::crew_view::brief_inputs(conn, child.id)?.unwrap();
            assert_eq!(brief.user_words, "Count the lines in notes.txt.");
            assert!(brief.user_words_trimmed);
            Ok(())
        })
        .await
        .unwrap();
    svc.shutdown().await;
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}
