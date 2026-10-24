use super::*;
use qm_store::retention::{LiveSession, OpenTask};
use qm_types::data_dir::private_folder;
use qm_types::domain::{EngineKind, TaskKind};
use qm_types::ids::{SecondmateId, SessionRef};
use std::os::unix::fs::PermissionsExt as _;

/// A throwaway data directory and scratch root, removed when dropped.
struct Home {
    base: PathBuf,
    data: DataDir,
}

impl Home {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("qm-retention-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for dir in [base.join("home"), base.join("scratch")] {
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let data = DataDir::with_scratch(base.join("home"), base.join("scratch"));
        Self { base, data }
    }
}

/// Writes a one-byte file, making its folders private, as the app's umask 077 would.
fn touch(path: &Path) {
    private_folder(path.parent().unwrap()).unwrap();
    fs::write(path, b"x").unwrap();
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn id(n: u32) -> TaskId {
    TaskId::new(n).unwrap()
}

fn open(n: u32, kind: TaskKind, builds_on: Option<u32>, retry_of: Option<u32>) -> OpenTask {
    OpenTask {
        id: id(n),
        kind,
        builds_on: builds_on.map(id),
        retry_of: retry_of.map(id),
    }
}

fn session(owner: Owner, name: &str) -> LiveSession {
    LiveSession {
        owner,
        engine: Some(EngineKind::Provider),
        session_ref: Some(SessionRef::new(name).unwrap()),
    }
}

#[test]
fn a_plan_stays_until_its_build_has_its_copy_and_evidence_until_no_one_reads_it() {
    let home = Home::new("plans");
    let data = &home.data;
    for n in [1, 2, 3, 4] {
        touch(&data.task(id(n)).plan.join("plan.md"));
        touch(&data.task(id(n)).evidence.join("run.log"));
    }
    // T-1 is a closed plan a build (T-5) hasn't copied yet; T-2 a closed plan T-6 has copied;
    // T-3 a closed task T-7 retries; T-4 closed and read by no one.
    let facts = Facts {
        retained_tasks: (1..=7).map(id).collect(),
        open: vec![
            open(5, TaskKind::Build, Some(1), None),
            open(6, TaskKind::Build, Some(2), None),
            open(7, TaskKind::Fix, None, Some(3)),
        ],
        sessions: vec![],
    };
    touch(&data.task(id(6)).plan.join("plan.md"));
    apply(data, &facts, SystemTime::now(), When::Running);

    assert!(
        data.task(id(1)).plan.join("plan.md").is_file(),
        "T-5 still needs it"
    );
    assert!(data.task(id(1)).evidence.is_dir(), "T-5 builds on T-1");
    assert!(!data.task(id(2)).plan.exists(), "T-6 has its copy");
    assert!(data.task(id(2)).evidence.is_dir(), "T-6 builds on T-2");
    assert!(!data.task(id(3)).plan.exists());
    assert!(data.task(id(3)).evidence.is_dir(), "T-7 retries T-3");
    assert!(
        !data.task(id(4)).artifacts.exists(),
        "nothing reads T-4: its folder goes"
    );
    assert!(
        data.task(id(6)).plan.join("plan.md").is_file(),
        "an open task keeps its own"
    );

    // Once the readers close, everything goes.
    apply(data, &Facts::default(), SystemTime::now(), When::Running);
    assert_eq!(fs::read_dir(data.artifacts_dir()).unwrap().count(), 0);
}

#[test]
fn transcripts_keep_the_named_session_and_unnamed_ones_wait_an_hour_unless_at_startup() {
    let home = Home::new("transcripts");
    let data = &home.data;
    let seat = data.seat().transcript;
    let mate = data.secondmate(SecondmateId::new(3).unwrap()).transcript;
    let task = data.task(id(8)).transcript;
    for dir in [&seat, &mate, &task] {
        touch(&dir.join("current.jsonl"));
        touch(&dir.join("superseded.jsonl"));
        touch(&dir.join(".current.jsonl.tmp-1-0"));
    }
    touch(&seat.join("notes.txt"));
    let facts = Facts {
        retained_tasks: vec![id(8)],
        open: vec![open(8, TaskKind::Build, None, None)],
        sessions: vec![
            session(Owner::Seat, "current"),
            session(Owner::Secondmate(SecondmateId::new(3).unwrap()), "current"),
            session(Owner::Task(id(8)), "current"),
        ],
    };
    // Running, everything is new: an unnamed file may be a session not yet recorded.
    apply(data, &facts, SystemTime::now(), When::Running);
    for dir in [&seat, &mate, &task] {
        assert!(dir.join("superseded.jsonl").is_file());
    }
    let later = SystemTime::now() + TEMP_AFTER + Duration::from_secs(1);
    apply(data, &facts, later, When::Running);
    for dir in [&seat, &mate, &task] {
        assert!(
            dir.join("current.jsonl").is_file(),
            "the named session stays"
        );
        assert!(!dir.join("superseded.jsonl").exists());
        assert!(!dir.join(".current.jsonl.tmp-1-0").exists());
    }
    assert!(
        seat.join("notes.txt").is_file(),
        "only transcripts and temporaries go"
    );

    touch(&seat.join("again.jsonl"));
    apply(data, &facts, SystemTime::now(), When::Startup);
    assert!(
        !seat.join("again.jsonl").exists(),
        "at startup nothing can be unrecorded"
    );
    assert!(seat.join("current.jsonl").is_file());
}

#[test]
fn the_sweep_never_follows_a_link() {
    let home = Home::new("links");
    let data = &home.data;
    let outside = home.base.join("outside");
    touch(&outside.join("precious"));
    private_folder(&data.artifacts_dir()).unwrap();
    std::os::unix::fs::symlink(&outside, data.artifacts_dir().join("T-0009")).unwrap();
    let evidence = data.task(id(10)).evidence;
    private_folder(evidence.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &evidence).unwrap();
    let transcript = data.seat().transcript;
    private_folder(&transcript).unwrap();
    std::os::unix::fs::symlink(outside.join("precious"), transcript.join("x.jsonl")).unwrap();

    apply(data, &Facts::default(), SystemTime::now(), When::Startup);
    assert!(
        outside.join("precious").is_file(),
        "nothing behind a link is removed"
    );
    assert!(
        fs::symlink_metadata(&evidence).is_err(),
        "the link itself goes"
    );
    assert!(fs::symlink_metadata(transcript.join("x.jsonl")).is_err());
}

#[test]
fn an_older_layouts_scratch_plans_and_evidence_move_in_or_go() {
    let home = Home::new("legacy");
    let data = &home.data;
    touch(&data.legacy_plan(id(11)).join("plan.md"));
    touch(&data.legacy_evidence(id(11)).join("run.log"));
    touch(&data.legacy_plan(id(12)).join("plan.md"));
    touch(&data.legacy_evidence(id(12)).join("run.log"));
    touch(&data.task(id(11)).root.join("evidence/old.log"));
    touch(&data.scratch().join("scratchpad/T-0012/notes"));
    touch(&data.scratch().join("scratchpad/T-0011/notes"));
    let facts = Facts {
        retained_tasks: vec![id(11)],
        open: vec![open(11, TaskKind::Plan, None, None)],
        sessions: vec![],
    };
    apply(data, &facts, SystemTime::now(), When::Startup);

    let paths = data.task(id(11));
    assert!(paths.plan.join("plan.md").is_file());
    assert!(paths.evidence.join("run.log").is_file());
    assert!(
        fs::symlink_metadata(data.legacy_plan(id(11)))
            .unwrap()
            .file_type()
            .is_symlink(),
        "a link is left at the old path"
    );
    assert!(
        data.legacy_plan(id(11)).join("plan.md").is_file(),
        "and still opens"
    );
    assert!(
        paths.root.join("evidence").exists(),
        "a second evidence folder is a conflict: preserved, not merged"
    );
    assert!(fs::symlink_metadata(data.legacy_plan(id(12))).is_err());
    assert!(fs::symlink_metadata(data.legacy_evidence(id(12))).is_err());
    assert!(!data.scratch().join("scratchpad/T-0012").exists());
    assert!(data.scratch().join("scratchpad/T-0011/notes").is_file());

    // Once T-11 closes, its files and the old links go.
    apply(data, &Facts::default(), SystemTime::now(), When::Running);
    assert!(fs::symlink_metadata(data.legacy_plan(id(11))).is_err());
    assert!(!paths.artifacts.exists());
}

#[test]
fn plan_staging_and_temporaries_go_after_an_hour() {
    let home = Home::new("staging");
    let data = &home.data;
    let paths = data.task(id(13));
    touch(&paths.artifacts.join(format!("{PLAN_STAGING}1-0/plan.md")));
    touch(&paths.plan.join(".plan.md.tmp-1-0"));
    touch(&paths.plan.join("plan.md"));
    let facts = Facts {
        retained_tasks: vec![id(13)],
        open: vec![open(13, TaskKind::Plan, None, None)],
        sessions: vec![],
    };
    apply(data, &facts, SystemTime::now(), When::Running);
    assert!(paths.artifacts.join(format!("{PLAN_STAGING}1-0")).exists());
    let later = SystemTime::now() + TEMP_AFTER + Duration::from_secs(1);
    apply(data, &facts, later, When::Running);
    assert!(!paths.artifacts.join(format!("{PLAN_STAGING}1-0")).exists());
    assert!(!paths.plan.join(".plan.md.tmp-1-0").exists());
    assert!(paths.plan.join("plan.md").is_file());
}

#[test]
fn a_report_survives_a_closed_retained_task_but_goes_when_forgotten() {
    let home = Home::new("reports");
    let data = &home.data;
    let paths = data.task(id(14));
    let report = paths.artifacts.join("report");
    touch(&report.join("report.html"));
    touch(&report.join("assets/chart.svg"));
    touch(&paths.plan.join("plan.md"));
    touch(&paths.evidence.join("run.log"));
    // The task is closed (not in `open`), but its row is still retained.
    let facts = Facts {
        retained_tasks: vec![id(14)],
        ..Facts::default()
    };
    for when in [When::Running, When::Startup] {
        apply(data, &facts, SystemTime::now(), when);
        assert!(report.join("report.html").is_file());
        assert!(report.join("assets/chart.svg").is_file());
        assert!(!paths.plan.exists());
        assert!(!paths.evidence.exists());
    }

    // Forgetting removes the row: no old report may remain for a reused task ID.
    apply(data, &Facts::default(), SystemTime::now(), When::Running);
    assert!(!report.exists());
    assert!(!paths.artifacts.exists());
}
