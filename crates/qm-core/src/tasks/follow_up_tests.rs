//! A follow-up crewmate joins the task of the work it builds on or retries, through the real start
//! transaction.
use super::*;
use crate::turns::forget_tests::Fixture;
use qm_types::domain::{StartedBy, TaskKind};
use qm_types::ids::TurnId;

fn qm() -> Caller {
    Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: None,
        task: None,
        secondmate: None,
        plan: None,
    }
}

fn request(title: &str) -> StartRequest {
    StartRequest {
        folder: None,
        project: None,
        in_place: false,
        create_folder: false,
        label: None,
        name: None,
        kind: TaskKind::Research,
        title: format!("test(core): {title}"),
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

/// Sets a crewmate's status directly, as its work would have.
async fn set_status(f: &Fixture, task: TaskId, status: &'static str) {
    f.db()
        .call(move |conn| {
            let closed = status == "closed";
            conn.execute(
                "UPDATE tasks SET status = ?2,
                     outcome = CASE WHEN ?3 THEN 'answered' END,
                     closed_at = CASE WHEN ?3 THEN 2 END
                 WHERE id = ?1",
                rusqlite::params![task, status, closed],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Starts `req` as `caller`, which must succeed.
async fn start(svc: &TaskService, caller: &Caller, req: StartRequest) -> TaskStarted {
    svc.start_crewmate(caller, req).await.unwrap()
}

/// `title`, building on `source`.
fn building_on(source: TaskId, title: &str) -> StartRequest {
    StartRequest {
        builds_on: Some(source),
        ..request(title)
    }
}

#[tokio::test]
async fn a_follow_up_joins_the_task_of_the_work_it_builds_on() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let first = start(&svc, &qm(), request("reproduce the bug")).await;
    assert_eq!(first.task, first.crewmate.own_label());

    // builds_on, with no join_task: the same task, not a new one.
    let fix = start(&svc, &qm(), building_on(first.crewmate, "fix the bug")).await;
    assert_eq!(fix.task, first.task);

    // A follow-up of the follow-up stays there too: the task, not the crewmate, is followed.
    let review = start(&svc, &qm(), building_on(fix.crewmate, "check the fix")).await;
    assert_eq!(review.task, first.task);

    // Settling the first crewmate leaves the task open while the others work in it.
    set_status(&f, first.crewmate, "closed").await;
    let later = start(&svc, &qm(), building_on(first.crewmate, "follow up")).await;
    assert_eq!(later.task, first.task);

    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn a_stronger_retry_joins_the_task_of_the_work_it_retries() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let first = start(&svc, &qm(), request("check the fix")).await;
    set_status(&f, first.crewmate, "failed").await;
    f.db()
        .call(|conn| {
            conn.execute_batch(
                "INSERT INTO placements (canonical, tier, created_at, seq)
                 VALUES ('stronger', 'premium', 0, 900);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    // A stronger retry needs the user's own turn.
    let on_user_turn = Caller {
        turn: Some(qm_tools::handler::TurnCx {
            turn: TurnId::new(1),
            started_by: StartedBy::User {
                message: MessageId::new(1),
            },
        }),
        ..qm()
    };
    let retry = start(
        &svc,
        &on_user_turn,
        StartRequest {
            stronger_retry_of: Some(first.crewmate),
            ..request("check the fix again")
        },
    )
    .await;
    assert_eq!(retry.task, first.task);

    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn retry_card_guidance_is_rendered_without_original_source_words() {
    const ORIGINAL: &str = "Original user prompt: count every line in notes.txt.";
    const GUIDANCE: &str = "Check the empty-file case too.";
    const INSTRUCTION: &str = "Explain how you checked the answer";

    for (original, words) in [
        (ORIGINAL, Some("  Check the empty-file case too. \n")),
        ("", Some(GUIDANCE)),
        (ORIGINAL, None),
        (ORIGINAL, Some(" \n\t ")),
    ] {
        let f = Fixture::new().await;
        let (svc, scheduler) = f.task_service();
        let first = start(
            &svc,
            &qm(),
            StartRequest {
                instructions: vec![INSTRUCTION.into()],
                ..request("retry with direct guidance")
            },
        )
        .await;
        set_status(&f, first.crewmate, "failed").await;
        f.db()
            .call(move |conn| {
                conn.execute(
                    "UPDATE tasks SET user_words = ?2 WHERE id = ?1",
                    rusqlite::params![first.crewmate, original],
                )?;
                conn.execute_batch(
                    "INSERT INTO placements (canonical, tier, created_at, seq)
                     VALUES ('stronger', 'premium', 0, 900);",
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let retry = svc
            .start_retry(
                first.crewmate,
                qm_types::domain::Tier::Premium,
                words.map(str::to_owned),
            )
            .await
            .unwrap();
        assert_eq!(retry.task, first.task);
        let mut inputs = svc
            .db()
            .call(move |conn| qm_store::crew_view::brief_inputs(conn, retry.crewmate))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inputs.goal, "Answer the question");
        assert_eq!(inputs.instructions[0], INSTRUCTION);
        assert_eq!(inputs.user_words.contains(ORIGINAL), !original.is_empty());
        // Queued tasks have not acquired runtime ports or a branch base yet.
        inputs.port_base = Some(41_000);
        inputs.base_ref = Some("origin/main".into());
        inputs.base_commit = Some("a".repeat(40).parse().unwrap());
        inputs.branch = Some("qm/test".into());
        let facts = crate::briefs::brief_facts(&inputs, None).unwrap();
        let rendered = qm_prompts::brief::render_brief(&facts);
        assert!(rendered.contains("Answer the question"), "{rendered}");
        assert!(rendered.contains(INSTRUCTION), "{rendered}");
        assert!(!rendered.contains(ORIGINAL), "{rendered}");
        assert!(!rendered.contains("## The user's own words"), "{rendered}");
        if words.is_some_and(|w| !w.trim().is_empty()) {
            assert_eq!(inputs.instructions.len(), 2);
            assert_eq!(
                inputs.instructions[1],
                format!(
                    "Direct user retry guidance (decision card for {}): {GUIDANCE}",
                    first.crewmate
                )
            );
            assert!(
                rendered.contains("Direct user retry guidance"),
                "{rendered}"
            );
            assert!(rendered.contains(GUIDANCE), "{rendered}");
        } else {
            assert_eq!(inputs.instructions, vec![INSTRUCTION.to_owned()]);
            assert!(
                !rendered.contains("Direct user retry guidance"),
                "{rendered}"
            );
            assert!(!rendered.contains(GUIDANCE), "{rendered}");
        }

        drop(svc);
        scheduler.abort();
        let _ = scheduler.await;
    }
}

#[tokio::test]
async fn join_task_wins_and_a_finished_task_gives_a_task_of_its_own() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let first = start(&svc, &qm(), request("reproduce the bug")).await;
    let other = start(&svc, &qm(), request("an unrelated question")).await;

    // An explicit join_task wins over the default.
    let joined = start(
        &svc,
        &qm(),
        StartRequest {
            label: Some(other.task),
            ..building_on(first.crewmate, "related to both")
        },
    )
    .await;
    assert_eq!(joined.task, other.task);

    // Once every crewmate in a task is closed, a follow-up gets a task of its own instead of being
    // refused.
    set_status(&f, other.crewmate, "closed").await;
    set_status(&f, joined.crewmate, "closed").await;
    let alone = start(
        &svc,
        &qm(),
        building_on(other.crewmate, "after it finished"),
    )
    .await;
    assert_eq!(alone.task, alone.crewmate.own_label());
    assert_ne!(alone.task, other.task);

    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}
