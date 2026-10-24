use super::*;
use crate::turns::forget_tests::Fixture;
use qm_types::{domain::TaskKind, ids::AgentId};

mod pruned_words;

fn caller() -> Caller {
    Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        turn: None,
        task: None,
        secondmate: None,
        plan: None,
    }
}
fn request() -> StartRequest {
    StartRequest {
        folder: None,
        project: None,
        in_place: false,
        create_folder: false,
        label: None,
        kind: TaskKind::Research,
        title: "test(core): investigate follow-up".into(),
        goal: "Answer the question".into(),
        constraints: vec![],
        tests_required: false,
        builds_on: None,
        instructions: vec!["Keep it narrow".into()],
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
    }
}
fn preference() -> qm_types::model_preference::ModelPreference {
    qm_types::model_preference::ModelPreference {
        selection: qm_types::ids::ModelId::new("frozen-model").unwrap(),
        provider: None,
        model: None,
        effort: Some(qm_types::domain::Effort::High),
    }
}

fn future() -> StartTrigger {
    StartTrigger::At {
        due: "2099-01-01T00:00:00Z".parse().unwrap(),
    }
}

#[tokio::test]
async fn registering_and_pulsing_before_due_allocates_nothing_and_cancel_is_final() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let scheduled = svc
        .schedule_crewmate(&caller(), request(), future())
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    f.db()
        .call(|conn| {
            let tasks: i64 = conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?;
            assert_eq!(tasks, 2, "only fixture tasks; no child before the trigger");
            let rows = deferred::list(conn, None, true)?;
            assert_eq!(rows.len(), 1);
            let saved: Saved = decode("deferred_starts.payload", &rows[0].payload)?;
            assert_eq!(saved.request, request(), "the full request round trips");
            Ok(())
        })
        .await
        .unwrap();
    let mut other = caller();
    other.agent = AgentId::new(2);
    assert_eq!(svc.list_crewmate_starts(&other).await.unwrap(), []);
    assert!(
        svc.cancel_crewmate_start(&other, scheduled.schedule)
            .await
            .is_err()
    );
    svc.cancel_crewmate_start(&caller(), scheduled.schedule)
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    assert_eq!(
        svc.list_crewmate_starts(&caller()).await.unwrap()[0].status,
        "cancelled"
    );
    assert!(
        svc.cancel_crewmate_start(&caller(), scheduled.schedule)
            .await
            .is_err()
    );
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

async fn fire_choice_after_restart(
    svc: &TaskService,
    message: MessageId,
    use_tier: bool,
) -> TaskService {
    let mut req = request();
    req.project = Some("p".into());
    req.choice_message = Some(message);
    if use_tier {
        req.tier = Some(qm_types::domain::Tier::High);
    } else {
        req.preference = Some(preference());
    }
    let mut foreign = caller();
    foreign.agent = AgentId::new(2);
    assert!(
        svc.schedule_crewmate(&foreign, req.clone(), future())
            .await
            .is_err()
    );
    assert!(svc.start_crewmate(&foreign, req.clone()).await.is_err());
    let expected = req.clone();
    let scheduled = svc
        .schedule_crewmate(
            &caller(),
            req,
            StartTrigger::At {
                due: Timestamp::UNIX_EPOCH,
            },
        )
        .await
        .unwrap();
    let id = scheduled.schedule;
    svc.db()
        .call(move |conn| {
            let row = deferred::get(conn, id)?.unwrap();
            let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
            assert_eq!(saved.request, expected);
            assert_eq!(saved.authority(), Authority::Turn(StartedBy::AppEvent));
            conn.execute("DELETE FROM messages WHERE id = ?1", [message])?;
            Ok(())
        })
        .await
        .unwrap();
    let restarted = reopened_service(svc);
    pulse(&restarted).await.unwrap();
    let row = restarted
        .list_crewmate_starts(&caller())
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.schedule == id)
        .unwrap();
    assert_eq!(row.status, "started", "{row:?}");
    let task = row.task.unwrap();
    restarted
        .db()
        .call(move |conn| {
            let (_, event) =
                qm_store::tasks::last_event(conn, task, start::STARTED_EVENT)?.unwrap();
            assert_eq!(
                event["route"]["choice_message"],
                serde_json::to_value(message).unwrap()
            );
            if use_tier {
                assert_eq!(event["route"]["named_tier"], "high");
            } else {
                assert_eq!(
                    event["route"]["preference"],
                    serde_json::to_value(preference()).unwrap()
                );
            }
            Ok(())
        })
        .await
        .unwrap();
    restarted
}

#[tokio::test]
async fn tier_and_preference_keep_original_sources_and_fire_after_chat_deletion() {
    let f = Fixture::new().await;
    let (mut svc, scheduler) = f.task_service();
    prepare_scoped_repo(&f, &svc).await;
    let message = svc
        .db()
        .call(|conn| {
            qm_store::messages::insert(
                conn,
                &qm_store::messages::NewMessage {
                    agent: caller().agent,
                    source: qm_types::domain::MessageSource::You,
                    kind: qm_types::domain::MessageKind::Message,
                    text: "Use the large tier and prefer the frozen model",
                    options: &[],
                    replies_to: None,
                    turn: None,
                    at: Timestamp::UNIX_EPOCH,
                },
            )
        })
        .await
        .unwrap();
    svc.db()
        .call(move |conn| {
            for _ in 0..=qm_store::user_message_sources::RECENT_SOURCES {
                qm_store::messages::insert(
                    conn,
                    &qm_store::messages::NewMessage {
                        agent: caller().agent,
                        source: qm_types::domain::MessageSource::You,
                        kind: qm_types::domain::MessageKind::Message,
                        text: "A later unrelated request",
                        options: &[],
                        replies_to: None,
                        turn: None,
                        at: Timestamp::UNIX_EPOCH,
                    },
                )?;
            }
            assert!(
                !qm_store::user_message_sources::recent(conn, caller().agent)?
                    .iter()
                    .any(|s| s.id == message)
            );
            Ok(())
        })
        .await
        .unwrap();
    for use_tier in [true, false] {
        svc = fire_choice_after_restart(&svc, message, use_tier).await;
    }
    // Guidance alone is allowed on an app-event turn; a tier without a source is not.
    let mut req = request();
    req.preference = Some(preference());
    svc.schedule_crewmate(&caller(), req.clone(), future())
        .await
        .unwrap();
    req.tier = Some(qm_types::domain::Tier::High);
    assert!(
        svc.schedule_crewmate(&caller(), req, future())
            .await
            .is_err()
    );
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn user_authority_and_words_are_frozen_without_granting_app_turn_overrides() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let mut req = request();
    req.in_place = true;
    req.choice_message = Some(MessageId::new(1));
    assert!(
        svc.schedule_crewmate(&caller(), req.clone(), future())
            .await
            .is_err(),
        "a citation cannot authorize in_place"
    );
    let mut user = caller();
    user.turn = Some(TurnCx {
        turn: TurnId::new(1),
        started_by: StartedBy::User {
            message: MessageId::new(1),
        },
    });
    svc.schedule_crewmate(&user, req, future()).await.unwrap();
    f.db()
        .call(|conn| {
            conn.execute("UPDATE messages SET text = 'changed after scheduling'", [])?;
            let row = deferred::list(conn, None, true)?.remove(0);
            let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
            assert_eq!(saved.words, "old words");
            assert_eq!(saved.words_message, Some(MessageId::new(1)));
            assert_eq!(
                saved.authority(),
                Authority::Turn(StartedBy::User {
                    message: MessageId::new(1)
                })
            );
            assert!(check_overrides(&saved.request, saved.authority(), saved.role).is_ok());
            Ok(())
        })
        .await
        .unwrap();
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn reused_task_identity_fails_once_and_wakes_owner_without_allocating_child() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let task = TaskId::new(2).unwrap();
    svc.schedule_crewmate(&caller(), request(), StartTrigger::Done { task })
        .await
        .unwrap();
    f.db()
        .call(|conn| {
            conn.execute("UPDATE tasks SET seq = seq + 100 WHERE id = 2", [])?;
            Ok(())
        })
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    pulse(&svc).await.unwrap();
    assert_eq!(
        svc.list_crewmate_starts(&caller()).await.unwrap()[0].status,
        "failed"
    );
    f.db()
        .call(|conn| {
            let wakes: i64 = conn.query_row(
                "SELECT count(*) FROM wakes WHERE dedupe_key LIKE 'deferred-start:%:failed'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(wakes, 1);
            let tasks: i64 = conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?;
            assert_eq!(tasks, 2);
            Ok(())
        })
        .await
        .unwrap();
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn done_requires_accepted_done_report_not_final_text_or_blocked_and_landed_is_separate() {
    let f = Fixture::new().await;
    f.db().call(|conn| {
        let task = TaskId::new(2).unwrap();
        let done = StartTrigger::Done { task };
        assert!(!ready_in_store(conn, &done, Timestamp::now())?);
        conn.execute_batch("INSERT INTO reports (task_id, agent_id, source, outcome, headings, created_at, seq) VALUES (2, 1, 'report_call', 'blocked', '{}', 1, 10);")?;
        assert!(!ready_in_store(conn, &done, Timestamp::now())?);
        conn.execute_batch("INSERT INTO reports (task_id, agent_id, source, outcome, headings, created_at, seq) VALUES (2, 1, 'report_call', 'done', '{}', 2, 11);")?;
        assert!(ready_in_store(conn, &done, Timestamp::now())?);
        let landed = StartTrigger::Landed { task };
        assert!(!ready_in_store(conn, &landed, Timestamp::now())?);
        conn.execute("UPDATE tasks SET status = 'landed' WHERE id = 2", [])?;
        assert!(ready_in_store(conn, &landed, Timestamp::now())?);
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn commit_observes_own_local_work_before_report_not_inherited_or_upstream_head() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let (svc, opened) = waiting_service(&svc);
    let (dir, worktree, base) = prepare_commit_repo(&f, &svc).await;
    let builder = TaskId::new(2).unwrap();
    let mut review_request = request();
    review_request.project = Some("p".into());
    review_request.kind = TaskKind::Review;
    review_request.builds_on = Some(builder);
    // Immediate starts must not weaken the commit requirement, either.
    assert!(
        svc.start_crewmate(&caller(), review_request.clone())
            .await
            .is_err()
    );
    svc.schedule_crewmate(
        &caller(),
        review_request,
        StartTrigger::Commit { task: builder },
    )
    .await
    .unwrap();
    pulse(&svc).await.unwrap();
    assert_eq!(
        svc.list_crewmate_starts(&caller()).await.unwrap()[0].status,
        "pending"
    );
    f.db()
        .call(|conn| {
            let count: i64 = conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?;
            assert_eq!(count, 2, "no child exists before a local task commit");
            Ok(())
        })
        .await
        .unwrap();
    assert!(committed(&svc, builder).await.unwrap().is_none());
    git(
        &dir,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-m",
            "upstream",
        ],
    );
    let upstream = git(&dir, &["rev-parse", "HEAD"]);
    // Fast-forward the task to upstream: changed HEAD is not task-owned work.
    git(
        &dir,
        &[
            "-C",
            worktree.to_str().unwrap(),
            "merge",
            "--ff-only",
            "main",
        ],
    );
    assert!(
        committed(&svc, TaskId::new(2).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    git(
        &dir,
        &[
            "-C",
            worktree.to_str().unwrap(),
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-m",
            "own work",
        ],
    );
    assert!(
        committed(&svc, TaskId::new(2).unwrap())
            .await
            .unwrap()
            .is_some()
    );
    let exact = git(
        &dir,
        &["-C", worktree.to_str().unwrap(), "rev-parse", "HEAD"],
    );
    pulse(&svc).await.unwrap();
    let scheduled = svc.list_crewmate_starts(&caller()).await.unwrap().remove(0);
    assert_eq!(scheduled.status, "started", "{scheduled:?}");
    let reviewer = scheduled.task.unwrap();
    reject_cross_project(&svc, &dir, builder).await;
    let newer = advance_and_reroute(&f, &svc, &dir, &worktree, reviewer, &exact, &opened).await;
    check_review_verdicts(&f, &svc, reviewer, exact, newer).await;
    assert_ne!(base, upstream);
    svc.shutdown().await;
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

async fn prepare_commit_repo(
    f: &Fixture,
    svc: &TaskService,
) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let dir = svc.deps().home.join("trigger-repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-b", "main"]);
    git(
        &dir,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    let base = git(&dir, &["rev-parse", "HEAD"]);
    git(&dir, &["switch", "-c", "crew"]);
    let worktree = svc
        .deps()
        .data_dir
        .task(TaskId::new(2).unwrap(), TaskId::new(2).unwrap().own_label())
        .worktree;
    std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
    // The app observes the task's actual registered worktree, not the user's checkout.
    git(&dir, &["switch", "main"]);
    git(
        &dir,
        &["worktree", "add", worktree.to_str().unwrap(), "crew"],
    );
    let path = dir.clone();
    let base_copy = base.clone();
    f.db()
        .call(move |conn| {
            conn.execute(
                "UPDATE projects SET path = ?1, delivery_mode = 'branch_only' WHERE id = 1",
                [path.to_str().unwrap()],
            )?;
            conn.execute(
                "UPDATE tasks SET branch = 'crew', base_ref = 'main', base_commit = ?1, status = 'running' WHERE id = 2",
                [base_copy],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (dir, worktree, base)
}

async fn reject_cross_project(svc: &TaskService, dir: &std::path::Path, builder: TaskId) {
    // A valid source SHA in one repository must not be used as another project's base.
    let other = svc.deps().home.join("other-repo");
    git(dir, &["init", "-b", "main", other.to_str().unwrap()]);
    git(
        dir,
        &[
            "-C",
            other.to_str().unwrap(),
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    let mut cross_project = request();
    cross_project.folder = Some(other.to_string_lossy().into_owned());
    cross_project.kind = TaskKind::Review;
    cross_project.builds_on = Some(builder);
    let refused = svc
        .start_crewmate(&caller(), cross_project.clone())
        .await
        .unwrap_err();
    assert!(
        refused.render().contains("same repository"),
        "{}",
        refused.render()
    );
    let cross_schedule = svc
        .schedule_crewmate(
            &caller(),
            cross_project,
            StartTrigger::Commit { task: builder },
        )
        .await
        .unwrap();
    pulse(svc).await.unwrap();
    let schedules = svc.list_crewmate_starts(&caller()).await.unwrap();
    let cross = schedules
        .iter()
        .find(|r| r.schedule == cross_schedule.schedule)
        .unwrap();
    assert_eq!(cross.status, "failed");
    assert!(cross.note.as_ref().unwrap().contains("same repository"));
}

async fn advance_and_reroute(
    f: &Fixture,
    svc: &TaskService,
    dir: &std::path::Path,
    worktree: &std::path::Path,
    reviewer: TaskId,
    exact: &str,
    opened: &tokio::sync::Notify,
) -> String {
    let builder = TaskId::new(2).unwrap();
    let pinned = exact.to_owned();
    f.db()
        .call(move |conn| {
            let row = qm_store::tasks::load(conn, reviewer)?.unwrap();
            assert_eq!(row.status, TaskStatus::Queued, "no model is available yet");
            assert_eq!(row.base_commit.unwrap().as_str(), pinned);
            assert!(
                qm_store::tasks::load(conn, builder)?
                    .unwrap()
                    .head_commit
                    .is_none(),
                "observing a commit did not fabricate a report"
            );
            Ok(())
        })
        .await
        .unwrap();
    // Both the builder and the default branch advance while this review waits for a model.
    std::fs::write(worktree.join("later.txt"), "not in the reviewed snapshot").unwrap();
    git(dir, &["-C", worktree.to_str().unwrap(), "add", "later.txt"]);
    git(
        dir,
        &[
            "-C",
            worktree.to_str().unwrap(),
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "feat: later work",
        ],
    );
    let newer = git(
        dir,
        &["-C", worktree.to_str().unwrap(), "rev-parse", "HEAD"],
    );
    git(
        dir,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-m",
            "feat: later upstream",
        ],
    );
    let newer_copy = newer.clone();
    f.db().call(move |conn| {
        conn.execute("UPDATE tasks SET head_commit = ?1 WHERE id = 2", [newer_copy])?;
        conn.execute_batch(r#"INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq) VALUES (101, 'anthropic', 'A', 'api_key', 'provider', 0, 101);
        INSERT INTO models (account_id, model, canonical, family, display_name, capabilities, efforts, first_seen_at, is_new, seq) VALUES
        (101, 'claude-test', 'claude-test', 'claude', 'Test', '{"image_input":true,"context_window":1000000,"generation":2,"known":true,"source":"account"}', '["low","medium","high"]', 0, 0, 102);
        INSERT INTO placements (canonical, tier, created_at, seq) VALUES ('claude-test', 'low', 0, 103), ('claude-test', 'medium', 0, 104), ('claude-test', 'high', 0, 105);"#)?;
        Ok(())
    }).await.unwrap();
    let brief = crate::briefs::builds_on(f.db(), &svc.deps().data_dir, reviewer)
        .await
        .unwrap()
        .unwrap();
    assert!(brief.fact.contains(exact), "{}", brief.fact);
    assert!(!brief.fact.contains(&newer));
    assert!(
        !brief.body.contains("later.txt"),
        "diff uses the frozen SHA too"
    );
    start::reroute_tick(svc).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), opened.notified())
        .await
        .expect("the runner prepared and opened its session");
    let review_worktree = svc
        .deps()
        .data_dir
        .task(reviewer, reviewer.own_label())
        .worktree;
    assert_eq!(
        git(
            dir,
            &["-C", review_worktree.to_str().unwrap(), "rev-parse", "HEAD"]
        ),
        exact
    );
    let pinned = exact.to_owned();
    f.db()
        .call(move |conn| {
            assert_eq!(
                qm_store::tasks::load(conn, reviewer)?
                    .unwrap()
                    .base_commit
                    .unwrap()
                    .as_str(),
                pinned
            );
            Ok(())
        })
        .await
        .unwrap();
    newer
}

async fn check_review_verdicts(
    f: &Fixture,
    svc: &TaskService,
    reviewer: TaskId,
    exact: String,
    newer: String,
) {
    let builder = TaskId::new(2).unwrap();
    let verification =
        crate::verification::VerificationService::new(crate::verification::VerificationDeps {
            db: f.db().clone(),
            clock: svc.clock_arc(),
            data_dir: svc.deps().data_dir.clone(),
            health: svc.deps().health.clone(),
            inbox: svc.deps().inbox.clone(),
            chat: svc.deps().chat.clone(),
        });
    f.db().call(move |conn| {
        use qm_store::{reports, tasks, verification as facts};
        use crate::verification::review::{passed_for, REVIEW_STARTED_EVENT};
        let old: qm_types::ids::CommitSha = exact.parse().unwrap();
        let new: qm_types::ids::CommitSha = newer.parse().unwrap();
        conn.execute("UPDATE tasks SET status = 'in_review', verification_level = 'reviewed', verification_source = 'user' WHERE id = 2", [])?;
        conn.execute("UPDATE tasks SET status = 'reported' WHERE id = ?1", [reviewer])?;
        let row = tasks::load(conn, reviewer)?.unwrap();
        let agent = qm_store::agents::crewmate_of(conn, reviewer)?.unwrap().id;
        let mut report = reports::NewReport::empty(reviewer, agent, Timestamp::UNIX_EPOCH);
        report.outcome = Some(ReportOutcome::Done);
        report.review_verdict = Some(qm_types::domain::ReviewVerdict::Pass);
        reports::insert(conn, &report)?;
        let report = reports::latest_clear(conn, reviewer)?.unwrap();
        let note = verification.review_reported(conn, &row, &report, &mut crate::inbox::Notify::new(), Timestamp::UNIX_EPOCH)?;
        assert!(note.note_for_supervisor.unwrap().contains("not"));
        assert!(!passed_for(conn, builder, &new)?, "an old review never approves a newer builder head");
        assert_eq!(facts::facts(conn, builder)?.unwrap().review_rounds, 0);
        conn.execute("UPDATE tasks SET head_commit = ?1 WHERE id = 2", [&old])?;
        verification.review_reported(conn, &row, &report, &mut crate::inbox::Notify::new(), Timestamp::UNIX_EPOCH)?;
        assert!(passed_for(conn, builder, &old)?);
        assert!(!passed_for(conn, builder, &new)?);
        // The existing reviewer is reused only by explicitly pinning its next review round.
        conn.execute("UPDATE tasks SET status = 'reported', head_commit = ?1 WHERE id = 2", [&new])?;
        let builder_row = tasks::load(conn, builder)?.unwrap();
        crate::verification::VerificationService::builder_reported(conn, &builder_row, &mut crate::inbox::Notify::new(), Timestamp::UNIX_EPOCH)?;
        assert!(!passed_for(conn, builder, &old)?);
        assert_eq!(tasks::load(conn, reviewer)?.unwrap().base_commit, Some(new.clone()));
        let (_, payload) = tasks::last_event(conn, builder, REVIEW_STARTED_EVENT)?.unwrap();
        assert_eq!(payload["head"], new.as_str(), "re-review wake/event keep the full exact SHA");
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn simultaneous_fires_create_one_child_through_real_start_transaction() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    f.db().call(|conn| {
        conn.execute_batch(r#"INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq) VALUES (101, 'anthropic', 'A', 'api_key', 'provider', 0, 101);
        INSERT INTO models (account_id, model, canonical, family, display_name, capabilities, efforts, first_seen_at, is_new, seq) VALUES
        (101, 'claude-test', 'claude-test', 'claude', 'Test', '{"image_input":true,"context_window":1000000,"generation":2,"known":true,"source":"account"}', '["low","medium","high"]', 0, 0, 102);
        INSERT INTO placements (canonical, tier, created_at, seq) VALUES ('claude-test', 'low', 0, 103), ('claude-test', 'medium', 0, 104), ('claude-test', 'high', 0, 105);"#)?;
        Ok(())
    }).await.unwrap();
    svc.schedule_crewmate(
        &caller(),
        request(),
        StartTrigger::At {
            due: Timestamp::UNIX_EPOCH,
        },
    )
    .await
    .unwrap();
    let (a, b) = tokio::join!(pulse(&svc), pulse(&svc));
    a.unwrap();
    b.unwrap();
    let rows = svc.list_crewmate_starts(&caller()).await.unwrap();
    assert_eq!(rows[0].status, "started", "{:?}", rows[0]);
    assert!(rows[0].task.is_some());
    pulse(&svc).await.unwrap();
    f.db()
        .call(|conn| {
            let tasks: i64 = conn.query_row(
                "SELECT count(*) FROM tasks WHERE title = 'test(core): investigate follow-up'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(
                tasks, 1,
                "dedup is the task-record transaction, not the pulse loop"
            );
            Ok(())
        })
        .await
        .unwrap();
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

async fn prepare_scoped_repo(f: &Fixture, svc: &TaskService) {
    let (dir, _, _) = prepare_commit_repo(f, svc).await;
    let remote = svc.deps().home.join("scoped-remote.git");
    git(
        &dir,
        &[
            "clone",
            "--bare",
            dir.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
    svc.db()
        .call(|conn| {
            conn.execute(
                "UPDATE projects SET delivery_mode = 'direct_pr' WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn secondmate(f: &Fixture, svc: &TaskService) -> (Caller, qm_types::ids::CorrelationId) {
    let data = svc.deps().data_dir.clone();
    f.db().call(move |conn| {
        use qm_store::{messages, secondmates};
        use qm_types::domain::{Effort, MessageKind, MessageSource};
        let at = Timestamp::UNIX_EPOCH;
        let sm = secondmates::insert(conn, &secondmates::NewSecondmate {
            name: "deferred", scope: "test", projects: &[], standing_branch: Some("main"),
            parent: AgentId::new(1), data_dir: &data, at,
        })?;
        let message = messages::insert(conn, &messages::NewMessage {
            agent: AgentId::new(1), source: MessageSource::You, kind: MessageKind::Message,
            text: "Use the frozen model with high effort and premium, then review carefully",
            options: &[], replies_to: None, turn: None, at,
        })?;
        conn.execute("UPDATE turns SET message_id = ?1 WHERE id = 1", [message])?;
        conn.execute_batch("INSERT INTO placements (canonical, tier, created_at, seq) VALUES ('frozen-model', 'premium', 0, 300);")?;
        let overrides = secondmates::RequestOverrides {
            model: Some(qm_types::ids::ModelId::new("frozen-model").unwrap()), effort: Some(Effort::High), premium: true,
            choice_message: None,
            tier: Some(qm_types::domain::Tier::Premium),
            preference: Some(preference()),
            later_steps: vec![secondmates::StoredLaterStep {
                kind: TaskKind::Review, model: None, effort: Some(Effort::High), premium: true,
            }],
        };
        let correlation = secondmates::insert_request(conn, &secondmates::NewRequest {
            secondmate: sm.id, turn: TurnId::new(1), request: "Do the work",
            user_words: "frozen words", instructions: &[], base_branch: Some("main"),
            overrides: &overrides, at,
        })?;
        Ok((Caller { agent: sm.agent, role: Role::Secondmate, secondmate: Some(sm.id),
            turn: None, task: None, plan: None }, correlation))
    }).await.unwrap()
}

fn service_deps(svc: &TaskService) -> crate::tasks::TaskDeps {
    let d = svc.deps();
    crate::tasks::TaskDeps {
        db: d.db.clone(),
        clock: d.clock.clone(),
        data_dir: d.data_dir.clone(),
        home: d.home.clone(),
        engines: d.engines.clone(),
        supervisor: d.supervisor.clone(),
        inbox: d.inbox.clone(),
        scheduler: d.scheduler.clone(),
        config: d.config.clone(),
        health: d.health.clone(),
        git: d.git.clone(),
        chat: d.chat.clone(),
        seat_agent: d.seat_agent,
        probe: d.probe.clone(),
        gate: d.gate.clone(),
        choices: d.choices.clone(),
        start_hook: d.start_hook.clone(),
        inherited_env: d.inherited_env.clone(),
        dispatcher: d.dispatcher,
        monitor: d.monitor.clone(),
    }
}

fn reopened_service(svc: &TaskService) -> TaskService {
    let mut deps = service_deps(svc);
    deps.db = qm_store::db::Db::open(&deps.home.join("state.db")).unwrap();
    TaskService::new(deps)
}

#[tokio::test]
async fn child_guidance_preserves_inherited_authority_for_immediate_and_deferred_starts() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    prepare_scoped_repo(&f, &svc).await;
    let (sm, correlation) = secondmate(&f, &svc).await;
    let mut req = request();
    req.project = Some("p".into());
    req.correlation = Some(correlation);
    let guidance = qm_types::model_preference::ModelPreference {
        selection: qm_types::ids::ModelId::new("child-guidance").unwrap(),
        ..preference()
    };
    req.preference = Some(guidance.clone());
    let immediate = svc.start_crewmate(&sm, req.clone()).await.unwrap();
    let scheduled = svc
        .schedule_crewmate(
            &sm,
            req,
            StartTrigger::At {
                due: Timestamp::UNIX_EPOCH,
            },
        )
        .await
        .unwrap();
    let id = scheduled.schedule;
    let frozen = svc
        .db()
        .call(move |conn| {
            let row = deferred::get(conn, id)?.unwrap();
            let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
            assert!(frozen_request(conn, id, &sm, &saved.request)?.is_some());
            let mut changed = saved.request.clone();
            changed.preference = None;
            assert!(
                frozen_request(conn, id, &sm, &changed)?.is_none(),
                "full replay equality still includes guidance"
            );
            // Force routing to rely on the frozen delegation rather than the live request.
            conn.execute(
                "UPDATE secondmate_requests SET overrides = '{}' WHERE id = ?1",
                [correlation],
            )?;
            Ok(saved.request)
        })
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    let row = svc.list_crewmate_starts(&sm).await.unwrap().remove(0);
    assert_eq!(row.status, "started", "{row:?}");
    svc.db()
        .call(move |conn| {
            for task in [immediate.task, row.task.unwrap()] {
                assert_eq!(
                    qm_store::tasks::load(conn, task)?.unwrap().status,
                    TaskStatus::Queued
                );
                let (_, event) =
                    qm_store::tasks::last_event(conn, task, start::STARTED_EVENT)?.unwrap();
                assert_eq!(event["route"]["named_model"], "frozen-model");
                assert_eq!(event["route"]["named_tier"], "premium");
                assert_eq!(
                    event["route"]["preference"],
                    serde_json::to_value(&guidance).unwrap()
                );
            }
            let mut facts = crate::routing::CrewmateFacts {
                task: row.task.unwrap(),
                agent: None,
                authority: Authority::Turn(StartedBy::AppEvent),
                kind: qm_routing::decider::Decided::Answer {
                    value: TaskKind::Research,
                    by: qm_routing::decider::DecidedBy::Declared,
                },
                decider: qm_routing::decider::DecidedBy::Declared,
                builds_on: None,
                needs: qm_routing::rank::Needs::default(),
                quick_verification: false,
                named_model: frozen.model,
                named_tier: frozen.tier,
                named_effort: frozen.effort,
                premium: frozen.premium,
                choice_message: frozen.choice_message,
                preference: None,
                stronger_retry: None,
                later_step: None,
                prefer_provider_other_than: None,
            };
            let message = facts.choice_message.unwrap();
            assert!(verifies_delegated_choice(conn, &facts, message)?);
            facts.named_model = None;
            assert!(!verifies_delegated_choice(conn, &facts, message)?);
            facts.named_model = Some(qm_types::ids::ModelId::new("frozen-model").unwrap());
            facts.named_tier = Some(qm_types::domain::Tier::High);
            assert!(!verifies_delegated_choice(conn, &facts, message)?);
            Ok(())
        })
        .await
        .unwrap();
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn delegated_config_and_source_survive_routing_turn_deletion_and_restart() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    prepare_scoped_repo(&f, &svc).await;
    let (sm, correlation) = secondmate(&f, &svc).await;
    let mut req = request();
    req.project = Some("p".into());
    req.correlation = Some(correlation); // No model fields and no source supplied by the child.
    let scheduled = svc
        .schedule_crewmate(
            &sm,
            req,
            StartTrigger::At {
                due: Timestamp::UNIX_EPOCH,
            },
        )
        .await
        .unwrap();
    let id = scheduled.schedule;
    let expected = svc
        .db()
        .call(move |conn| {
            let row = deferred::get(conn, id)?.unwrap();
            let saved: Saved = decode("deferred_starts.payload", &row.payload)?;
            assert!(saved.delegated.is_some());
            assert_eq!(
                saved.request.model.as_ref().unwrap().as_str(),
                "frozen-model"
            );
            assert_eq!(saved.request.later_steps.len(), 1);
            conn.execute("DELETE FROM session_lines WHERE turn_id = 1", [])?;
            conn.execute(
                "UPDATE secondmate_requests SET turn_id = NULL WHERE turn_id = 1",
                [],
            )?;
            conn.execute("DELETE FROM turns WHERE id = 1", [])?;
            // Later edits must not rewrite the registered intent, including its source.
            conn.execute(
                "UPDATE secondmate_requests SET overrides = '{}' WHERE id = ?1",
                [correlation],
            )?;
            conn.execute("UPDATE secondmates SET standing_base_branch = 'crew'", [])?;
            Ok(saved.request)
        })
        .await
        .unwrap();
    let restarted = reopened_service(&svc);
    pulse(&restarted).await.unwrap();
    let row = restarted.list_crewmate_starts(&sm).await.unwrap().remove(0);
    assert_eq!(row.status, "started", "{row:?}");
    let task = row.task.unwrap();
    restarted
        .db()
        .call(move |conn| {
            let child = qm_store::tasks::load(conn, task)?.unwrap();
            assert_eq!(child.user_words, "frozen words");
            assert_eq!(child.base_branch.as_deref(), Some("main"));
            assert_eq!(
                child.later_steps,
                serde_json::to_value(expected.later_steps).unwrap()
            );
            let (_, event) =
                qm_store::tasks::last_event(conn, task, start::STARTED_EVENT)?.unwrap();
            assert_eq!(event["route"]["named_model"], "frozen-model");
            assert_eq!(event["route"]["named_tier"], "premium");
            assert_eq!(
                event["route"]["preference"],
                serde_json::to_value(preference()).unwrap()
            );
            assert_eq!(event["route"]["named_effort"], "high");
            assert_eq!(event["route"]["premium"], true);
            assert_eq!(
                event["route"]["choice_message"],
                serde_json::to_value(expected.choice_message).unwrap()
            );
            Ok(())
        })
        .await
        .unwrap();
    pulse(&restarted).await.unwrap();
    drop(restarted);
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn standing_base_is_frozen_and_matching_typed_base_is_not_an_override() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    prepare_scoped_repo(&f, &svc).await;
    let (sm, _) = secondmate(&f, &svc).await;
    let mut req = request();
    req.project = Some("p".into());
    req.base_branch = Some("origin/main".into());
    let mut wrong = req.clone();
    wrong.base_branch = Some("crew".into());
    assert!(matches!(
        svc.schedule_crewmate(&sm, wrong, future()).await,
        Err(ToolFailure::Refused(_))
    ));
    assert!(matches!(
        svc.schedule_crewmate(&caller(), req.clone(), future())
            .await,
        Err(ToolFailure::Refused(_))
    ));
    svc.schedule_crewmate(
        &sm,
        req,
        StartTrigger::At {
            due: Timestamp::UNIX_EPOCH,
        },
    )
    .await
    .unwrap();
    svc.db()
        .call(|conn| {
            conn.execute("UPDATE secondmates SET standing_base_branch = 'crew'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    let row = svc.list_crewmate_starts(&sm).await.unwrap().remove(0);
    assert_eq!(row.status, "started", "{row:?}");
    let task = row.task.unwrap();
    svc.db()
        .call(move |conn| {
            assert_eq!(
                qm_store::tasks::load(conn, task)?
                    .unwrap()
                    .base_branch
                    .as_deref(),
                Some("main")
            );
            Ok(())
        })
        .await
        .unwrap();
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn typed_parent_citation_cannot_borrow_delegation_for_extra_fields() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    let (sm, correlation) = secondmate(&f, &svc).await;
    let mut req = request();
    req.correlation = Some(correlation);
    req.model = Some(qm_types::ids::ModelId::new("different-model").unwrap());
    req.choice_message = svc
        .db()
        .call(|conn| {
            Ok(
                qm_store::user_message_sources::recent(conn, AgentId::new(1))?
                    .last()
                    .map(|m| m.id),
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        svc.schedule_crewmate(&sm, req, future()).await,
        Err(ToolFailure::Refused(_))
    ));
    let other_source = svc
        .db()
        .call(|conn| {
            qm_store::messages::insert(
                conn,
                &qm_store::messages::NewMessage {
                    agent: AgentId::new(1),
                    source: qm_types::domain::MessageSource::You,
                    kind: qm_types::domain::MessageKind::Message,
                    text: "An unrelated parent request",
                    options: &[],
                    replies_to: None,
                    turn: None,
                    at: Timestamp::UNIX_EPOCH,
                },
            )
        })
        .await
        .unwrap();
    let mut borrowed = request();
    borrowed.correlation = Some(correlation);
    borrowed.choice_message = Some(other_source);
    assert!(matches!(
        svc.schedule_crewmate(&sm, borrowed, future()).await,
        Err(ToolFailure::Refused(_))
    ));
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn failed_task_transaction_retries_but_does_not_post_a_terminal_wake() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    svc.schedule_crewmate(
        &caller(),
        request(),
        StartTrigger::At {
            due: Timestamp::UNIX_EPOCH,
        },
    )
    .await
    .unwrap();
    svc.db().call(|conn| {
        conn.execute_batch("CREATE TEMP TRIGGER fail_deferred_start BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'injected record failure'); END;")?;
        Ok(())
    }).await.unwrap();
    pulse(&svc).await.unwrap();
    assert_eq!(
        svc.list_crewmate_starts(&caller()).await.unwrap()[0].status,
        "pending"
    );
    svc.db()
        .call(|conn| {
            let wakes: i64 = conn.query_row(
                "SELECT count(*) FROM wakes WHERE dedupe_key LIKE 'deferred-start:%:failed'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(wakes, 0);
            conn.execute_batch("DROP TRIGGER fail_deferred_start;")?;
            Ok(())
        })
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    assert_eq!(
        svc.list_crewmate_starts(&caller()).await.unwrap()[0].status,
        "started"
    );
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

async fn check_failed_mirror_after_commit(routed: bool) {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    if routed {
        svc.db().call(|conn| {
            conn.execute_batch(r#"INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq) VALUES (101, 'anthropic', 'A', 'api_key', 'provider', 0, 101);
            INSERT INTO models (account_id, model, canonical, family, display_name, capabilities, efforts, first_seen_at, is_new, seq) VALUES
            (101, 'claude-test', 'claude-test', 'claude', 'Test', '{"image_input":true,"context_window":1000000,"generation":2,"known":true,"source":"account"}', '["low","medium","high"]', 0, 0, 102);
            INSERT INTO placements (canonical, tier, created_at, seq) VALUES ('claude-test', 'medium', 0, 103);"#)?;
            Ok(())
        }).await.unwrap();
    }
    let scheduled = svc
        .schedule_crewmate(
            &caller(),
            request(),
            StartTrigger::At {
                due: Timestamp::UNIX_EPOCH,
            },
        )
        .await
        .unwrap();
    // This trigger exists only on disk: the in-memory task transaction commits successfully,
    // then Db::call reports Failed because mirroring that new task aborts.
    let disk = Connection::open(svc.deps().home.join("state.db")).unwrap();
    disk.execute_batch("CREATE TRIGGER fail_launch_mirror BEFORE INSERT ON tasks WHEN NEW.title = 'test(core): investigate follow-up' BEGIN SELECT RAISE(ABORT, 'injected mirror failure'); END;").unwrap();
    pulse(&svc).await.unwrap();
    disk.execute_batch("DROP TRIGGER fail_launch_mirror;")
        .unwrap();
    let id = scheduled.schedule;
    let task = svc
        .db()
        .call(move |conn| {
            let row = deferred::get(conn, id)?.unwrap();
            assert_eq!(row.status, "started");
            assert!(
                row.note.is_none(),
                "launch dispatch was skipped after the mirror error"
            );
            let task = row.task.unwrap();
            assert_eq!(
                qm_store::tasks::load(conn, task)?.unwrap().status,
                if routed {
                    TaskStatus::Preparing
                } else {
                    TaskStatus::Queued
                }
            );
            Ok(task)
        })
        .await
        .unwrap();
    pulse(&svc).await.unwrap();
    pulse(&svc).await.unwrap();
    svc.db()
        .call(move |conn| {
            let row = deferred::get(conn, id)?.unwrap();
            assert_eq!(row.task, Some(task));
            assert_eq!(row.note.as_deref(), Some("Launch dispatched."));
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM tasks WHERE title = 'test(core): investigate follow-up'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 1);
            Ok(())
        })
        .await
        .unwrap();
    drop(svc);
    scheduler.abort();
    let _ = scheduler.await;
}

#[tokio::test]
async fn failed_mirror_after_commit_recovers_the_existing_queued_launch_on_the_next_pulse() {
    check_failed_mirror_after_commit(false).await;
}

#[tokio::test]
async fn failed_mirror_after_commit_recovers_the_existing_preparing_runner_on_the_next_pulse() {
    check_failed_mirror_after_commit(true).await;
}

// Preparing manually after reroute_tick races the runner's failure/retirement cleanup.
fn waiting_service(svc: &TaskService) -> (TaskService, std::sync::Arc<tokio::sync::Notify>) {
    let opened = std::sync::Arc::new(tokio::sync::Notify::new());
    let mut deps = service_deps(svc);
    deps.engines.insert(
        qm_types::domain::EngineKind::Provider,
        std::sync::Arc::new(WaitingEngine(opened.clone())),
    );
    (TaskService::new(deps), opened)
}

/// Holds the real runner alive after preparation, without credentials or a provider.
#[derive(Debug)]
struct WaitingEngine(std::sync::Arc<tokio::sync::Notify>);
impl qm_engine::Engine for WaitingEngine {
    fn kind(&self) -> qm_types::domain::EngineKind {
        qm_types::domain::EngineKind::Provider
    }
    fn open(
        &self,
        _: qm_engine::SessionSpec,
    ) -> Result<qm_engine::SessionHandle, qm_engine::EngineError> {
        let (handle, mut side) = qm_engine::session_channel();
        self.0.notify_one();
        tokio::spawn(async move {
            while let Some(command) = side.commands.recv().await {
                if matches!(
                    command,
                    qm_engine::SessionCommand::Interrupt
                        | qm_engine::SessionCommand::Close
                        | qm_engine::SessionCommand::Forget
                ) {
                    let _ = side
                        .events
                        .send(qm_engine::EngineEvent::SessionClosed {
                            reason: qm_engine::CloseReason::Requested,
                        })
                        .await;
                    break;
                }
            }
        });
        Ok(handle)
    }
    fn models<'a>(
        &'a self,
        _: &'a qm_engine::AccountRef,
    ) -> qm_types::BoxFuture<'a, Result<Vec<qm_engine::DetectedModel>, qm_engine::EngineError>>
    {
        Box::pin(async { Ok(vec![]) })
    }
    fn usage<'a>(
        &'a self,
        _: &'a qm_engine::AccountRef,
    ) -> qm_types::BoxFuture<'a, Result<Option<qm_engine::UsageReading>, qm_engine::EngineError>>
    {
        Box::pin(async { Ok(None) })
    }
}

/// Runs after asynchronous base settlement, before the task-record transaction.
#[derive(Debug)]
struct WithdrawDone {
    db: qm_store::db::Db,
    withdrawn: std::sync::atomic::AtomicBool,
}
impl crate::tasks::delivery_probe::DeliveryProbe for WithdrawDone {
    fn probe<'a>(
        &'a self,
        _: &'a crate::tasks::ProjectRow,
        _: &'a std::path::Path,
    ) -> qm_types::BoxFuture<'a, crate::tasks::delivery_probe::DeliveryReadiness> {
        Box::pin(async move {
            if self
                .withdrawn
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return crate::tasks::delivery_probe::DeliveryReadiness::ready();
            }
            self.db
                .call(|conn| {
                    let trigger = StartTrigger::Done {
                        task: TaskId::new(2).unwrap(),
                    };
                    assert!(ready_in_store(conn, &trigger, Timestamp::UNIX_EPOCH)?);
                    let mut report = qm_store::reports::NewReport::empty(
                        TaskId::new(2).unwrap(),
                        AgentId::new(1),
                        Timestamp::UNIX_EPOCH,
                    );
                    report.outcome = Some(ReportOutcome::Blocked);
                    qm_store::reports::insert(conn, &report)?;
                    Ok(())
                })
                .await
                .unwrap();
            crate::tasks::delivery_probe::DeliveryReadiness::ready()
        })
    }
}

#[tokio::test]
async fn withdrawn_done_during_settlement_waits_then_starts_exactly_once() {
    let f = Fixture::new().await;
    let (svc, scheduler) = f.task_service();
    prepare_scoped_repo(&f, &svc).await;
    let mut deps = service_deps(&svc);
    deps.probe = std::sync::Arc::new(WithdrawDone {
        db: f.db().clone(),
        withdrawn: std::sync::atomic::AtomicBool::new(false),
    });
    let settling = TaskService::new(deps);
    f.db().call(|conn| {
        conn.execute_batch("INSERT INTO reports (task_id, agent_id, source, outcome, headings, created_at, seq) VALUES (2, 1, 'report_call', 'done', '{}', 2, 11);")?;
        Ok(())
    }).await.unwrap();
    let mut req = request();
    req.project = Some("p".into());
    req.kind = TaskKind::Build;
    settling
        .schedule_crewmate(
            &caller(),
            req,
            StartTrigger::Done {
                task: TaskId::new(2).unwrap(),
            },
        )
        .await
        .unwrap();
    pulse(&settling).await.unwrap();
    pulse(&settling).await.unwrap();
    let row = settling
        .list_crewmate_starts(&caller())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(row.status, "pending", "{row:?}");
    assert!(row.task.is_none());
    f.db().call(|conn| {
        let counts: (i64, i64, i64, i64) = conn.query_row("SELECT (SELECT count(*) FROM tasks), (SELECT count(*) FROM agents), (SELECT count(*) FROM routing_decisions), (SELECT count(*) FROM wakes WHERE dedupe_key LIKE 'deferred-start:%:failed')", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        assert_eq!(counts, (2, 2, 0, 0), "waiting allocated nothing and posted no terminal wake");
        let mut report = qm_store::reports::NewReport::empty(TaskId::new(2).unwrap(), AgentId::new(1), Timestamp::UNIX_EPOCH);
        report.outcome = Some(ReportOutcome::Done);
        qm_store::reports::insert(conn, &report)?;
        Ok(())
    }).await.unwrap();
    let (a, b) = tokio::join!(pulse(&settling), pulse(&settling));
    a.unwrap();
    b.unwrap();
    pulse(&settling).await.unwrap();
    let row = settling
        .list_crewmate_starts(&caller())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(row.status, "started", "{row:?}");
    f.db()
        .call(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM tasks WHERE title = 'test(core): investigate follow-up'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 1);
            Ok(())
        })
        .await
        .unwrap();
    settling.shutdown().await;
    svc.shutdown().await;
    scheduler.abort();
    let _ = scheduler.await;
}
