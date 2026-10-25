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
        folder_label: id(n).own_label(),
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
        touch(&data.task(id(n), id(n).own_label()).plan.join("plan.md"));
        touch(&data.task(id(n), id(n).own_label()).evidence.join("run.log"));
    }
    // T-1 is a closed plan a build (T-5) hasn't copied yet; T-2 a closed plan T-6 has copied;
    // T-3 a closed task T-7 retries; T-4 closed and read by no one.
    let facts = Facts {
        unread_reports: vec![],
        open: vec![
            open(5, TaskKind::Build, Some(1), None),
            open(6, TaskKind::Build, Some(2), None),
            open(7, TaskKind::Fix, None, Some(3)),
        ],
        sessions: vec![],
    };
    touch(&data.task(id(6), id(6).own_label()).plan.join("plan.md"));
    apply(data, &facts, SystemTime::now(), When::Running);

    assert!(
        data.task(id(1), id(1).own_label())
            .plan
            .join("plan.md")
            .is_file(),
        "T-5 still needs it"
    );
    assert!(
        data.task(id(1), id(1).own_label()).evidence.is_dir(),
        "T-5 builds on T-1"
    );
    assert!(
        !data.task(id(2), id(2).own_label()).plan.exists(),
        "T-6 has its copy"
    );
    assert!(
        data.task(id(2), id(2).own_label()).evidence.is_dir(),
        "T-6 builds on T-2"
    );
    assert!(!data.task(id(3), id(3).own_label()).plan.exists());
    assert!(
        data.task(id(3), id(3).own_label()).evidence.is_dir(),
        "T-7 retries T-3"
    );
    assert!(
        !data.task(id(4), id(4).own_label()).artifacts.exists(),
        "nothing reads T-4: its folder goes"
    );
    assert!(
        data.task(id(6), id(6).own_label())
            .plan
            .join("plan.md")
            .is_file(),
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
    let task = data.task(id(8), id(8).own_label()).transcript;
    for dir in [&seat, &mate, &task] {
        touch(&dir.join("current.jsonl"));
        touch(&dir.join("superseded.jsonl"));
        touch(&dir.join(".current.jsonl.tmp-1-0"));
    }
    touch(&seat.join("notes.txt"));
    let facts = Facts {
        unread_reports: vec![],
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
    let evidence = data.task(id(10), id(10).own_label()).evidence;
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
    touch(
        &data
            .task(id(11), id(11).own_label())
            .root
            .join("evidence/old.log"),
    );
    touch(&data.scratch().join("scratchpad/T-0012/notes"));
    touch(&data.scratch().join("scratchpad/T-0011/notes"));
    let facts = Facts {
        unread_reports: vec![],
        open: vec![open(11, TaskKind::Plan, None, None)],
        sessions: vec![],
    };
    apply(data, &facts, SystemTime::now(), When::Startup);

    let paths = data.task(id(11), id(11).own_label());
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
    assert!(
        data.scratch()
            .join("scratchpad/T-0011/C-0011/notes")
            .is_file(),
        "an open crewmate's old scratchpad is filed under its task"
    );

    // Once T-11 closes, its files and the old links go.
    apply(data, &Facts::default(), SystemTime::now(), When::Running);
    assert!(fs::symlink_metadata(data.legacy_plan(id(11))).is_err());
    assert!(!paths.artifacts.exists());
}

fn label(n: u32) -> TaskLabelId {
    id(n).own_label()
}

#[test]
fn an_older_versions_artifacts_are_filed_under_their_task() {
    let home = Home::new("filing");
    let data = &home.data;
    let flat = |n: u32| data.artifacts_dir().join(label(n).to_string());
    touch(&flat(15).join("plan/plan.md"));
    touch(&flat(15).join("evidence/run.log"));
    touch(&flat(15).join("report/report.html"));
    touch(&flat(16).join("report/report.html"));
    touch(&flat(17).join("plan/plan.md"));
    // C-15 is open, C-16 closed with its report's card unread, C-17 forgotten.
    let facts = Facts {
        unread_reports: vec![id(16)],
        open: vec![open(15, TaskKind::Plan, None, None)],
        sessions: vec![],
    };
    apply(data, &facts, SystemTime::now(), When::Startup);

    let c15 = data.task(id(15), label(15));
    assert!(c15.plan.join("plan.md").is_file());
    assert!(c15.evidence.join("run.log").is_file());
    assert!(c15.artifacts.join("report/report.html").is_file());
    assert!(
        is_link(&flat(15).join("plan")) && flat(15).join("plan/plan.md").is_file(),
        "an open crewmate keeps a link at each path it was given"
    );
    let c16 = data.task(id(16), label(16));
    assert!(c16.artifacts.join("report/report.html").is_file());
    assert!(
        fs::symlink_metadata(flat(16).join("report")).is_err(),
        "a closed crewmate's files move without a link"
    );
    assert!(fs::symlink_metadata(flat(17)).is_err());

    // Once C-15 closes, the links and all its files go; C-16's unread report stays.
    let closed = Facts {
        unread_reports: vec![id(16)],
        ..Facts::default()
    };
    apply(data, &closed, SystemTime::now(), When::Running);
    assert!(fs::symlink_metadata(flat(15).join("plan")).is_err());
    assert!(!c15.artifacts.exists());
    assert!(c16.artifacts.join("report/report.html").is_file());
}

#[test]
fn a_crewmate_moved_to_another_task_keeps_links_at_its_old_folders_until_it_closes() {
    let home = Home::new("relabel");
    let data = &home.data;
    let before = data.task(id(21), label(21));
    let after = data.task(id(21), label(20));
    touch(&before.plan.join("plan.md"));
    touch(&before.scratchpad.join("notes"));
    qm_types::data_dir::migrate_directory(&before.artifacts, &after.artifacts).unwrap();
    qm_types::data_dir::migrate_directory(&before.scratchpad, &after.scratchpad).unwrap();
    let mut moved = open(21, TaskKind::Plan, None, None);
    moved.folder_label = label(20);
    let facts = Facts {
        unread_reports: vec![],
        open: vec![moved],
        sessions: vec![],
    };
    for when in [When::Startup, When::Running] {
        apply(data, &facts, SystemTime::now(), when);
        assert!(after.plan.join("plan.md").is_file());
        assert!(after.scratchpad.join("notes").is_file());
        assert!(
            before.plan.join("plan.md").is_file() && before.scratchpad.join("notes").is_file(),
            "the old paths still open through their links"
        );
    }

    apply(data, &Facts::default(), SystemTime::now(), When::Running);
    for gone in [
        before.artifacts,
        before.scratchpad,
        after.artifacts,
        after.scratchpad,
    ] {
        assert!(fs::symlink_metadata(&gone).is_err(), "{}", gone.display());
    }
    assert!(fs::symlink_metadata(data.artifacts_dir().join("T-0021")).is_err());
    assert!(fs::symlink_metadata(data.scratchpads_dir().join("T-0021")).is_err());
}

#[test]
fn plan_staging_and_temporaries_go_after_an_hour() {
    let home = Home::new("staging");
    let data = &home.data;
    let paths = data.task(id(13), id(13).own_label());
    touch(&paths.artifacts.join(format!("{PLAN_STAGING}1-0/plan.md")));
    touch(&paths.plan.join(".plan.md.tmp-1-0"));
    touch(&paths.plan.join("plan.md"));
    let facts = Facts {
        unread_reports: vec![],
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
fn a_closed_tasks_report_stays_while_its_card_is_unread_then_goes_with_its_folder() {
    let home = Home::new("reports");
    let data = &home.data;
    let paths = data.task(id(14), id(14).own_label());
    let report = paths.artifacts.join("report");
    touch(&report.join("report.html"));
    touch(&report.join("assets/chart.svg"));
    touch(&paths.plan.join("plan.md"));
    touch(&paths.evidence.join("run.log"));
    // The task is closed (not in `open`), but you haven't opened or dismissed its report yet.
    let facts = Facts {
        unread_reports: vec![id(14)],
        ..Facts::default()
    };
    for when in [When::Running, When::Startup] {
        apply(data, &facts, SystemTime::now(), when);
        assert!(report.join("report.html").is_file());
        assert!(report.join("assets/chart.svg").is_file());
        assert!(!paths.plan.exists());
        assert!(!paths.evidence.exists());
    }

    // Once you have read it (or the task is forgotten), the report goes, and its folders with it.
    apply(data, &Facts::default(), SystemTime::now(), When::Running);
    assert!(!report.exists());
    assert!(!paths.artifacts.exists());
    assert!(fs::symlink_metadata(data.artifacts_dir().join("T-0014")).is_err());
}

#[test]
fn an_open_crewmates_report_stays_and_a_read_closed_ones_goes_at_once() {
    let home = Home::new("read-reports");
    let data = &home.data;
    let open_report = data
        .task(id(18), id(18).own_label())
        .artifacts
        .join("report");
    let read_report = data
        .task(id(19), id(19).own_label())
        .artifacts
        .join("report");
    for report in [&open_report, &read_report] {
        touch(&report.join("report.html"));
        touch(&report.join("template.html"));
    }
    // C-18 is still open (reported, waiting on its supervisor); C-19 closed and read.
    let facts = Facts {
        open: vec![open(18, TaskKind::Research, None, None)],
        ..Facts::default()
    };
    apply(data, &facts, SystemTime::now(), When::Running);
    assert!(open_report.join("report.html").is_file());
    assert!(!read_report.exists());
    assert!(!data.task(id(19), id(19).own_label()).artifacts.exists());
}

/// A clock whose sleeps end only when the test says so.
#[derive(Debug, Default)]
struct GatedClock {
    wake: Arc<tokio::sync::Notify>,
}

impl Clock for GatedClock {
    fn now(&self) -> jiff::Timestamp {
        jiff::Timestamp::UNIX_EPOCH
    }
    fn instant(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn sleep_until(&self, _: std::time::Instant) -> qm_types::clock::Sleep {
        let wake = Arc::clone(&self.wake);
        Box::pin(async move { wake.notified().await })
    }
}

#[tokio::test]
async fn an_opened_closed_report_goes_after_the_browser_had_time_to_load_it() {
    let home = Home::new("opened");
    let data = home.data.clone();
    let db = Db::open(&home.base.join("state.db")).unwrap();
    // C-22 closed with its answer, and you have just opened its report.
    db.call(|conn| {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode,
                 status, outcome, created_at, reported_at, report_read_at, closed_at, seq)
             VALUES (22, 1, 1, 'lookup', 't', 'g', 'folder', 'closed', 'answered', 1, 2, 3, 3, 1);",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let page = data
        .task(id(22), id(22).own_label())
        .artifacts
        .join("report/report.html");
    touch(&page);
    let clock = Arc::new(GatedClock::default());
    after_report_read(db, data.clone(), clock.clone(), true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(page.is_file(), "kept while the browser loads it");
    clock.wake.notify_one();
    for _ in 0..250 {
        if !page.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!page.exists());
    assert!(fs::symlink_metadata(data.artifacts_dir().join("T-0022")).is_err());
}
