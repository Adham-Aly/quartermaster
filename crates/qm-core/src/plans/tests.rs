use super::*;
use qm_types::domain::Role;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

/// A fresh private folder under the system's temporary folder, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("qm-plans-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        private_folder(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn create_makes_a_private_folder_once() {
    let scratch = Scratch::new("create");
    let dir = scratch.0.join("T-0001");
    let first = create(&dir).unwrap();
    assert_eq!(first.uid.len(), 24);
    assert_eq!(first.stage, Stage::Drafting);
    assert!(dir.join(ASSETS_DIR).is_dir());
    let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let state_mode = fs::metadata(dir.join(STATE_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(state_mode, 0o600);
    // A second call keeps the plan's id.
    assert_eq!(create(&dir).unwrap().uid, first.uid);
}

#[test]
fn update_changes_and_saves() {
    let scratch = Scratch::new("update");
    let dir = scratch.0.join("T-0002");
    assert!(update(&dir, |s| s.round = 1).unwrap().is_none());
    create(&dir).unwrap();
    let (state, before) = update(&dir, |s| s.session_mode.replace(PlanMode::Internal))
        .unwrap()
        .unwrap();
    assert_eq!(before, None);
    assert_eq!(state.session_mode, Some(PlanMode::Internal));
    assert_eq!(
        load(&dir).unwrap().unwrap().session_mode,
        Some(PlanMode::Internal)
    );
}

#[test]
fn a_build_gets_its_own_copy() {
    let scratch = Scratch::new("copy");
    let plan = scratch.0.join("T-0003");
    create(&plan).unwrap();
    fs::write(plan.join(PLAN_FILE), "# Plan\n").unwrap();
    fs::write(plan.join(ASSETS_DIR).join("a.png"), b"png").unwrap();
    prepare_template(&plan, PlanMode::Interactive).unwrap();
    fs::write(plan.join(PAGE_FILE), AUTHORED_HTML).unwrap();
    render(
        &plan,
        "T-0003".parse().unwrap(),
        "project",
        &PlanState::new(),
        BTreeMap::new(),
        None,
    )
    .unwrap();
    assert!(plan.join(AUTHOR_FILE).is_file());
    std::os::unix::fs::symlink("/etc/hosts", plan.join(ASSETS_DIR).join("link.png")).unwrap();
    let build = scratch.0.join("T-0004");
    copy_for_build(&plan, &build).unwrap();
    assert_eq!(
        fs::read_to_string(build.join(PLAN_FILE)).unwrap(),
        "# Plan\n"
    );
    assert_eq!(
        fs::read(build.join(ASSETS_DIR).join("a.png")).unwrap(),
        b"png"
    );
    // Only the technical handoff and assets belong in a build, never review UI or state.
    let mut entries: Vec<_> = fs::read_dir(&build)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, vec![ASSETS_DIR, PLAN_FILE]);
    // Links are never followed into the copy.
    assert!(!build.join(ASSETS_DIR).join("link.png").exists());
    // The copy outlives the plan's folder.
    fs::remove_dir_all(&plan).unwrap();
    assert!(build.join(PLAN_FILE).is_file());
}

#[test]
fn read_refuses_a_linked_plan() {
    let scratch = Scratch::new("link");
    let dir = scratch.0.join("T-0005");
    create(&dir).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", dir.join(PLAN_FILE)).unwrap();
    assert!(read(&dir).unwrap_err().contains("plain file"));
}

fn state(stage: Stage) -> PlanState {
    let mut s = PlanState::new();
    s.stage = stage;
    s.round = 2;
    s.open = vec!["a".into(), "b".into()];
    s
}

#[test]
fn the_card_follows_the_stage() {
    use qm_types::api::CardAction;
    let task: TaskId = "T-0044".parse().unwrap();
    let page = Path::new("/tmp/quartermaster/plans/T-0044/plan.html");
    let approve = |c: &qm_types::api::Card| {
        c.buttons
            .iter()
            .any(|b| matches!(b.action, CardAction::ApprovePlan { .. }))
    };

    let open = api::card(task, "plan(x): y", "proj", &state(Stage::WithUser), page);
    assert_eq!(open.heading, api::QUESTIONS_HEADING);
    assert!(!approve(&open));
    assert!(open.answered.is_none());

    let ready = api::card(task, "plan(x): y", "proj", &state(Stage::Final), page);
    assert_eq!(ready.heading, api::FINAL_HEADING);
    assert!(approve(&ready));

    // A reply on its way: no Approve until the planner's next round is back.
    let mut pending = state(Stage::Final);
    pending.pending_reply = true;
    let card = api::card(task, "plan(x): y", "proj", &pending, page);
    assert_eq!(card.heading, api::HEADING);
    assert!(!approve(&card));

    let mut locked = state(Stage::Final);
    locked.locked = Some("closed".into());
    assert!(!approve(&api::card(task, "t", "p", &locked, page)));

    let mut approved = state(Stage::Approved);
    approved.approved_at = Some("14:30".into());
    let card = api::card(task, "t", "p", &approved, page);
    assert_eq!(card.answered.as_deref(), Some("Approved 14:30"));
}

/// Nothing an auto planner is shown carries the question flow's words: its prompt addendum, its
/// skill, the shared crewmate prompts and the descriptions of every tool it is offered.
#[test]
fn an_auto_planner_sees_no_question_flow() {
    let mode = Some(PlanMode::Internal);
    let mut texts: Vec<(String, String)> = vec![
        (
            "crewmate-plan-internal".into(),
            qm_prompts::index::get("crewmate-plan-internal")
                .unwrap()
                .to_owned(),
        ),
        (
            "skill".into(),
            qm_plans::skill(PlanMode::Internal).to_owned(),
        ),
        (
            "crewmate".into(),
            qm_prompts::index::get("crewmate").unwrap().to_owned(),
        ),
    ];
    for d in qm_tools::registry::descriptors(Role::Crewmate, mode) {
        texts.push((d.name.clone(), d.description.clone()));
    }
    for (name, text) in texts {
        assert_eq!(
            qm_plans::interactive_marker(&text),
            None,
            "{name} shows an auto planner the question flow"
        );
    }
}

#[test]
fn a_state_file_with_the_old_chat_card_still_loads() {
    let mut value = serde_json::to_value(PlanState::new()).unwrap();
    value["card"] = serde_json::json!(12345);
    let state: PlanState = serde_json::from_value(value).unwrap();
    assert_eq!(state.card, Some(MessageId::new(12345)));
}

const TECHNICAL_MD: &str = "# Technical handoff\n\n## Implementation\n\nPrivate adapter migration details for the build agent.\n";
const AUTHORED_HTML: &str = r#"<article><h1>A friendlier cache</h1>
<h2 id="design">Design</h2><p class="author-prose">Keep your work close at hand.</p>
<pre data-question="">id = "store"
ask = "Which store?"
choices = [
 { id = "sqlite", label = "SQLite", recommended = true },
 { id = "sled", label = "sled" },
]</pre>
<h2 id="assumptions">Assumptions</h2><p>Local use only.</p></article>"#;

fn authored_plan(name: &str) -> (Scratch, PlanState, TaskId) {
    let scratch = Scratch::new(name);
    let state = create(&scratch.0).unwrap();
    fs::write(scratch.0.join(PLAN_FILE), TECHNICAL_MD).unwrap();
    fs::write(scratch.0.join(PAGE_FILE), AUTHORED_HTML).unwrap();
    (scratch, state, "T-0044".parse().unwrap())
}

#[test]
fn normal_template_does_not_manufacture_a_page_from_markdown() {
    let scratch = Scratch::new("template-only");
    let state = create(&scratch.0).unwrap();
    for name in ["template.html", PAGE_FILE, AUTHOR_FILE] {
        assert!(!scratch.0.join(name).exists());
    }
    fs::write(scratch.0.join(PLAN_FILE), TECHNICAL_MD).unwrap();
    prepare_template(&scratch.0, PlanMode::Interactive).unwrap();
    assert_eq!(
        fs::read_to_string(scratch.0.join("template.html")).unwrap(),
        qm_plans::authoring_template()
    );
    assert!(
        check(&scratch.0, PlanMode::Interactive, &state)
            .unwrap_err()
            .contains(PAGE_FILE)
    );
    assert!(
        render(
            &scratch.0,
            "T-0044".parse().unwrap(),
            "project",
            &state,
            BTreeMap::new(),
            None,
        )
        .is_err()
    );
    assert!(!scratch.0.join(PAGE_FILE).exists());
    assert!(!scratch.0.join(AUTHOR_FILE).exists());
}

#[test]
fn interactive_check_validates_technical_markdown_and_authored_html_separately() {
    let (scratch, state, _) = authored_plan("separate-checks");
    // Technical prose has neither questions nor assumptions: those belong to HTML.
    let checked = check(&scratch.0, PlanMode::Interactive, &state).unwrap();
    assert_eq!(checked.questions.len(), 1);
    assert_eq!(checked.questions[0].id, "store");

    fs::write(
        scratch.0.join(PLAN_FILE),
        "# Handoff\n\n```question\nid = \"technical-only\"\nask = \"Why?\"\n```\n",
    )
    .unwrap();
    assert!(
        check(&scratch.0, PlanMode::Interactive, &state)
            .unwrap_err()
            .contains(PLAN_FILE)
    );
    fs::write(scratch.0.join(PLAN_FILE), TECHNICAL_MD).unwrap();
    fs::write(scratch.0.join(PAGE_FILE), "<script>alert(1)</script>").unwrap();
    assert!(check(&scratch.0, PlanMode::Interactive, &state).is_err());
}

#[test]
fn check_render_check_render_preserves_independent_authored_prose() {
    let (scratch, state, task) = authored_plan("stable-cycle");
    let checked = check(&scratch.0, PlanMode::Interactive, &state).unwrap();
    let first = render(&scratch.0, task, "project", &state, BTreeMap::new(), None).unwrap();
    let page = fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap();
    assert!(page.contains("<p class=\"author-prose\">Keep your work close at hand.</p>"));
    assert!(!page.contains("Private adapter migration details"));
    assert!(!page.contains("Technical handoff"));
    assert_ne!(page, AUTHORED_HTML);
    let saved: AuthoredPage =
        serde_json::from_str(&fs::read_to_string(scratch.0.join(AUTHOR_FILE)).unwrap()).unwrap();
    assert_eq!(saved.source, AUTHORED_HTML);
    assert_eq!(saved.output, qm_plans::digest(&page, &Assets::default()));
    assert_eq!(authored_source(&scratch.0).unwrap(), AUTHORED_HTML);
    assert_eq!(first.checked, checked);
    assert_eq!(
        check(&scratch.0, PlanMode::Interactive, &state).unwrap(),
        checked
    );
    let second = render(&scratch.0, task, "project", &state, BTreeMap::new(), None).unwrap();
    assert_eq!(second.checked, checked);
    assert_eq!(second.digest, first.digest);
    assert_eq!(fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap(), page);
    assert_eq!(
        fs::read_to_string(scratch.0.join(PLAN_FILE)).unwrap(),
        TECHNICAL_MD
    );
}

#[test]
fn replacing_composed_output_starts_a_new_authored_revision() {
    let (scratch, state, task) = authored_plan("source-revisions");
    let first = render(&scratch.0, task, "project", &state, BTreeMap::new(), None).unwrap();
    let revised = AUTHORED_HTML.replace(
        "Keep your work close at hand.",
        "A revised human explanation.",
    );
    fs::write(scratch.0.join(PAGE_FILE), &revised).unwrap();
    // The old saved source must not win when the output fingerprint no longer matches.
    assert_eq!(authored_source(&scratch.0).unwrap(), revised);
    check(&scratch.0, PlanMode::Interactive, &state).unwrap();
    let second = render(&scratch.0, task, "project", &state, BTreeMap::new(), None).unwrap();
    assert_ne!(second.digest, first.digest);
    assert_eq!(authored_source(&scratch.0).unwrap(), revised);
    let page = fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap();
    assert!(page.contains("A revised human explanation."));
    assert!(!page.contains("Keep your work close at hand."));
    let third = render(&scratch.0, task, "project", &state, BTreeMap::new(), None).unwrap();
    assert_eq!(third.digest, second.digest);
    assert_eq!(fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap(), page);
}

#[test]
fn stage_and_port_rerenders_keep_the_authored_source() {
    let (scratch, mut state, task) = authored_plan("stage-port");
    state.stage = Stage::WithUser;
    state.round = 1;
    state.open = vec!["store".into()];
    state.page_token = PageToken::fresh();
    assert!(state.page_token.is_some());
    let first = render(
        &scratch.0,
        task,
        "project",
        &state,
        BTreeMap::new(),
        Some(41049),
    )
    .unwrap();
    first.record(&mut state);
    save(&scratch.0, &state).unwrap();
    assert_eq!(first.port, Some(41049));
    // Ports are only serialized into the page; no endpoint is started by this test.
    assert!(rerender(&scratch.0, task, "project", Some(41050)).unwrap());
    let page = fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap();
    assert!(page.contains("connect-src http://127.0.0.1:41050"));
    assert!(!page.contains("127.0.0.1:41049"));
    assert_eq!(authored_source(&scratch.0).unwrap(), AUTHORED_HTML);
    update(&scratch.0, |s| {
        s.stage = Stage::Approved;
        s.approved_at = Some("14:30".into());
    })
    .unwrap();
    assert!(rerender(&scratch.0, task, "project", Some(41050)).unwrap());
    let page = fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap();
    assert!(page.contains("14:30"));
    assert!(page.contains("Keep your work close at hand."));
    assert!(!page.contains("Private adapter migration details"));
    assert_eq!(authored_source(&scratch.0).unwrap(), AUTHORED_HTML);
    let saved = load(&scratch.0).unwrap().unwrap();
    assert_eq!(saved.rendered_digest, Some(first.digest));
    assert_eq!(saved.page_port, None);

    // A planner's unreported revision must not be overwritten by a runtime refresh.
    let revised = AUTHORED_HTML.replace("Local use only.", "Shared use now.");
    fs::write(scratch.0.join(PAGE_FILE), &revised).unwrap();
    assert!(!rerender(&scratch.0, task, "project", Some(41051)).unwrap());
    assert!(
        !render_saved(&scratch.0, task, "project", Some(41051), |s| s
            .fingerprints
            .clone())
        .unwrap()
    );
    assert_eq!(
        fs::read_to_string(scratch.0.join(PAGE_FILE)).unwrap(),
        revised
    );
}

#[test]
fn auto_preparation_removes_all_interactive_artifacts() {
    let (scratch, state, task) = authored_plan("auto-cleanup");
    prepare_template(&scratch.0, PlanMode::Interactive).unwrap();
    render(&scratch.0, task, "project", &state, BTreeMap::new(), None).unwrap();
    prepare_template(&scratch.0, PlanMode::Internal).unwrap();
    prepare_template(&scratch.0, PlanMode::Internal).unwrap();
    for name in ["template.html", PAGE_FILE, AUTHOR_FILE] {
        assert!(!scratch.0.join(name).exists(), "auto left {name}");
    }
    assert_eq!(
        fs::read_to_string(scratch.0.join(PLAN_FILE)).unwrap(),
        TECHNICAL_MD
    );
    assert!(scratch.0.join(ASSETS_DIR).is_dir());
    assert!(scratch.0.join(STATE_FILE).is_file());
    let auto_md = format!("{TECHNICAL_MD}\n## Decisions made without the user\n\nNone.\n");
    fs::write(scratch.0.join(PLAN_FILE), &auto_md).unwrap();
    check(&scratch.0, PlanMode::Internal, &state).unwrap();
    assert!(!scratch.0.join(PAGE_FILE).exists());
    fs::write(scratch.0.join(PAGE_FILE), AUTHORED_HTML).unwrap();
    assert!(
        check(&scratch.0, PlanMode::Internal, &state)
            .unwrap_err()
            .contains("remove it")
    );
}
