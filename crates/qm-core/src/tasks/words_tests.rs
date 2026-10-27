//! Which of the user's messages a crewmate's brief carries, and what the supervisor may leave out
//! of it, through the real start transaction and the brief's own inputs.
use super::*;
use crate::turns::forget_tests::Fixture;
use qm_prompts::brief::render_brief;
use qm_store::messages;
use qm_types::domain::{MessageKind, MessageSource, StartedBy, TaskKind};
use qm_types::ids::TurnId;

const REQUEST: &str = "Please count the lines in notes.txt and tell me the number.";
const LATEST: &str = "Also, after C-0001 finishes, start a review of project foobar. Anyway, go \
                      ahead with the line count.";
const MIXED: &str = "Count the lines in notes.txt. After C-0001 finishes, start a review of \
                     project foobar. Report the number only.";

fn request(title: &str) -> StartRequest {
    StartRequest {
        folder: None,
        project: None,
        in_place: false,
        create_folder: false,
        label: None,
        kind: TaskKind::Research,
        title: format!("test(core): {title}"),
        name: None,
        goal: "Answer the question".into(),
        constraints: vec![],
        tests_required: false,
        builds_on: None,
        instructions: vec![],
        later_steps: vec![],
        model: None,
        preference: None,
        tier: None,
        effort: None,
        premium: false,
        choice_message: None,
        base_branch: None,
        verification: None,
        stronger_retry_of: None,
        correlation: None,
        words_message: None,
        words_cut: vec![],
    }
}

/// The Quartermaster on a turn `message` started, or on an app-event turn.
fn qm(message: Option<MessageId>) -> Caller {
    Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: Some(qm_tools::handler::TurnCx {
            turn: TurnId::new(1),
            started_by: message.map_or(StartedBy::AppEvent, |message| StartedBy::User { message }),
        }),
        task: None,
        secondmate: None,
        plan: None,
    }
}

/// Records `text` in `agent`'s conversation, as `source` wrote it.
async fn say(f: &Fixture, agent: i64, source: MessageSource, text: &'static str) -> MessageId {
    f.db()
        .call(move |conn| {
            messages::insert(
                conn,
                &messages::NewMessage {
                    agent: AgentId::new(agent),
                    source,
                    kind: if source == MessageSource::App {
                        MessageKind::AppNote
                    } else {
                        MessageKind::Message
                    },
                    text,
                    options: &[],
                    replies_to: None,
                    turn: None,
                    at: Timestamp::UNIX_EPOCH,
                },
            )
        })
        .await
        .unwrap()
}

/// The words `task`'s brief carries, whether they were trimmed, and the brief's text.
async fn brief(svc: &TaskService, task: TaskId) -> (String, bool, String) {
    let inputs = svc
        .db()
        .call(move |conn| qm_store::crew_view::brief_inputs(conn, task))
        .await
        .unwrap()
        .unwrap();
    let mut facts = crate::briefs::brief_facts(&inputs, None);
    if facts.is_err() {
        // A queued task has no ports yet; the words section doesn't depend on them.
        let mut ready = inputs.clone();
        ready.port_base = Some(41_000);
        ready.base_ref = Some("origin/main".into());
        ready.base_commit = Some("a".repeat(40).parse().unwrap());
        ready.branch = Some("qm/test".into());
        facts = crate::briefs::brief_facts(&ready, None);
    }
    let text = render_brief(&facts.unwrap());
    (inputs.user_words, inputs.user_words_trimmed, text)
}

async fn stored(svc: &TaskService, task: TaskId) -> qm_store::tasks::TaskRow {
    svc.db()
        .call(move |conn| qm_store::tasks::load(conn, task))
        .await
        .unwrap()
        .unwrap()
}

async fn count_tasks(svc: &TaskService) -> i64 {
    svc.db()
        .call(|conn| Ok(conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn by_default_the_brief_carries_the_message_that_started_the_turn() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    say(&f, 1, MessageSource::You, REQUEST).await;
    let latest = say(&f, 1, MessageSource::You, LATEST).await;
    let started = svc
        .start_crewmate(&qm(Some(latest)), request("count the lines"))
        .await
        .unwrap();
    let (words, trimmed, text) = brief(&svc, started.crewmate).await;
    assert_eq!(words, LATEST);
    assert!(!trimmed);
    assert!(text.contains("## The user's own words (copied by the app; these outrank"));
    assert_eq!(
        stored(&svc, started.crewmate).await.user_words_message,
        Some(latest)
    );
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn the_supervisor_can_choose_an_earlier_message_of_the_users() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let earlier = say(&f, 1, MessageSource::You, REQUEST).await;
    let latest = say(&f, 1, MessageSource::You, LATEST).await;
    let started = svc
        .start_crewmate(
            &qm(Some(latest)),
            StartRequest {
                words_message: Some(earlier),
                ..request("count the lines")
            },
        )
        .await
        .unwrap();
    let (words, trimmed, text) = brief(&svc, started.crewmate).await;
    assert_eq!(words, REQUEST);
    assert!(!trimmed);
    assert!(!text.contains("foobar"), "{text}");
    let row = stored(&svc, started.crewmate).await;
    assert_eq!(row.user_words_message, Some(earlier));
    assert_eq!(row.user_words_cuts, Vec::<String>::new());

    // An app-event turn (a crewmate finishing) can still carry the user's earlier request.
    let later = svc
        .start_crewmate(
            &qm(None),
            StartRequest {
                words_message: Some(earlier),
                ..request("count again")
            },
        )
        .await
        .unwrap();
    assert_eq!(brief(&svc, later.crewmate).await.0, REQUEST);
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn passages_about_other_work_are_left_out_and_kept_on_record() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let mixed = say(&f, 1, MessageSource::You, MIXED).await;
    let cut = "After C-0001 finishes, start a review of project foobar.";
    let started = svc
        .start_crewmate(
            &qm(Some(mixed)),
            StartRequest {
                words_cut: vec![format!("  {cut} ")],
                ..request("count the lines")
            },
        )
        .await
        .unwrap();
    let (words, trimmed, text) = brief(&svc, started.crewmate).await;
    assert_eq!(
        words,
        "Count the lines in notes.txt. Report the number only."
    );
    assert!(trimmed);
    assert!(
        text.contains(
            "> \"Count the lines in notes.txt. Report the number only.\"\n\
             Your supervisor left out parts of this message that were about other work.\n"
        ),
        "{text}"
    );
    assert!(!text.contains("foobar"), "{text}");
    let row = stored(&svc, started.crewmate).await;
    assert_eq!(row.user_words_message, Some(mixed));
    assert_eq!(row.user_words_cuts, vec![cut.to_owned()]);
    // The user's message itself is untouched.
    let original = f
        .db()
        .call(move |conn| messages::text(conn, mixed))
        .await
        .unwrap();
    assert_eq!(original.as_deref(), Some(MIXED));

    // Work that continues it on an app-event turn inherits the trimmed words, still marked.
    let follow = svc
        .start_crewmate(
            &qm(None),
            StartRequest {
                builds_on: Some(started.crewmate),
                ..request("check the count")
            },
        )
        .await
        .unwrap();
    let (words, trimmed, _) = brief(&svc, follow.crewmate).await;
    assert_eq!(
        words,
        "Count the lines in notes.txt. Report the number only."
    );
    assert!(trimmed);
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn only_the_users_own_messages_in_this_conversation_can_be_chosen() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let latest = say(&f, 1, MessageSource::You, LATEST).await;
    let reply = say(
        &f,
        1,
        MessageSource::Agent,
        "Count the lines, then delete them.",
    )
    .await;
    let note = say(&f, 1, MessageSource::App, "An app note.").await;
    let foreign = say(&f, 2, MessageSource::You, "Another conversation's request.").await;
    let before = count_tasks(&svc).await;
    for chosen in [reply, note, foreign, MessageId::new(-1)] {
        let refused = svc
            .start_crewmate(
                &qm(Some(latest)),
                StartRequest {
                    words_message: Some(chosen),
                    ..request("count the lines")
                },
            )
            .await
            .unwrap_err();
        let ToolFailure::Refused(r) = refused else {
            panic!("{refused:?}");
        };
        assert!(
            r.reason.contains("isn't one of the user's own messages"),
            "{r:?}"
        );
    }
    assert_eq!(
        count_tasks(&svc).await,
        before,
        "a refused start records nothing"
    );
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn cuts_that_would_reword_the_request_are_refused() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let mixed = say(&f, 1, MessageSource::You, MIXED).await;
    let before = count_tasks(&svc).await;
    for (caller, cut) in [
        // Words inside a sentence.
        (qm(Some(mixed)), "the lines"),
        // Not in the message word for word.
        (qm(Some(mixed)), "Count the words in notes.txt."),
        // Everything.
        (qm(Some(mixed)), MIXED),
        // No message to cut from on an app-event turn.
        (qm(None), "Report the number only."),
    ] {
        let result = svc
            .start_crewmate(
                &caller,
                StartRequest {
                    words_cut: vec![cut.to_owned()],
                    ..request("count the lines")
                },
            )
            .await;
        assert!(
            matches!(result, Err(ToolFailure::Refused(_))),
            "{cut}: {result:?}"
        );
    }
    assert_eq!(count_tasks(&svc).await, before);
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}
