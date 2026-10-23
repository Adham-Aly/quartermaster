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
