//! Moving a crewmate's workplace, through the real task service, store, data directory and git:
//! between plain folders, into a repository's worktree and back, with the old branch kept only
//! when it has commits, a dirty worktree waiting, and the refusals.
use super::*;
use crate::turns::forget_tests::Fixture;
use qm_store::tasks as store;
use qm_tools::defs::{CrewmateMoved, MoveTiming, SetTaskProjectArgs};
use qm_types::domain::{DeliveryMode, TaskStatus};
use std::path::{Path, PathBuf};

const CREWMATE: i64 = 3;

fn task() -> TaskId {
    TaskId::new(2).unwrap()
}
fn qm() -> Caller {
    Caller {
        agent: AgentId::new(1),
        role: Role::Quartermaster,
        task: None,
        secondmate: None,
        turn: None,
        plan: None,
    }
}
fn to(folder: &Path) -> SetTaskProjectArgs {
    SetTaskProjectArgs {
        task: task(),
        project: None,
        folder: Some(folder.display().to_string()),
        in_place: false,
        create_folder: false,
        base_branch: None,
    }
}
fn refused(error: ToolFailure) -> String {
    let ToolFailure::Refused(r) = error else {
        panic!("expected a refusal, got {error:?}")
    };
    r.reason
}

fn git(dir: &Path, args: &[&str]) -> String {
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Crewmate C-2 (agent 3) runs under the Quartermaster, working in place in the plain folder
/// `a`; `b` is another plain folder and `repo` a local repository with one commit.
struct Move {
    fixture: Fixture,
    svc: TaskService,
    scheduler: JoinHandle<()>,
    a: PathBuf,
    b: PathBuf,
    repo: PathBuf,
}
impl Move {
    async fn new() -> Self {
        let fixture = Fixture::new().await;
        let (svc, scheduler) = fixture.task_service();
        let root = svc.deps().home.clone();
        let (a, b, repo) = (root.join("a"), root.join("b"), root.join("repo"));
        for dir in [&a, &b, &repo] {
            std::fs::create_dir_all(dir).unwrap();
        }
        git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("README"), "hello\n").unwrap();
        git(&repo, &["add", "README"]);
        git(
            &repo,
            &["-c", "commit.gpgSign=false", "commit", "-m", "base"],
        );
        let home = a.clone();
        fixture
            .db()
            .call(move |conn| {
                conn.execute(
                    "UPDATE projects SET name = 'a', path = ?1, delivery_mode = 'folder' WHERE id = 1",
                    [home.to_str().unwrap()],
                )?;
                conn.execute(
                    "UPDATE tasks SET status = 'running', in_place = 1, delivery_mode = 'folder',
                     branch = NULL, port_base = 41040 WHERE id = 2",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO agents (id, role, supervisor_id, task_id, home, status, created_at, seq)
                     VALUES (?1, 'crewmate', 1, 2, ?2, 'idle', 1, 8)",
                    rusqlite::params![CREWMATE, home.to_str().unwrap()],
                )?;
                qm_store::seq::observe_all(conn)?;
                Ok(())
            })
            .await
            .unwrap();
        Self {
            fixture,
            svc,
            scheduler,
            a: a.canonicalize().unwrap(),
            b: b.canonicalize().unwrap(),
            repo: repo.canonicalize().unwrap(),
        }
    }
    async fn moved(&self, args: SetTaskProjectArgs) -> CrewmateMoved {
        self.svc.set_task_project(&qm(), args).await.unwrap()
    }
    async fn refusal(&self, args: SetTaskProjectArgs) -> String {
        refused(self.svc.set_task_project(&qm(), args).await.unwrap_err())
    }
    async fn row(&self) -> store::TaskRow {
        self.fixture
            .db()
            .call(|conn| store::load(conn, task()))
            .await
            .unwrap()
            .unwrap()
    }
    async fn home(&self) -> PathBuf {
        self.fixture
            .db()
            .call(|conn| qm_store::agents::crewmate_of(conn, task()))
            .await
            .unwrap()
            .unwrap()
            .home
    }
    async fn pending(&self) -> Option<store::PendingMove> {
        self.fixture
            .db()
            .call(|conn| store::pending_move(conn, task()))
            .await
            .unwrap()
    }
    async fn project_name(&self) -> String {
        let project = self.row().await.project;
        self.fixture
            .db()
            .call(move |conn| qm_store::projects::load(conn, project))
            .await
            .unwrap()
            .unwrap()
            .name
    }
    /// Whether a project at `dir` is listed (not retired).
    async fn listed(&self, dir: &Path) -> bool {
        let dir = dir.to_str().unwrap().to_owned();
        self.fixture
            .db()
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM projects WHERE path = ?1 AND removed_at IS NULL",
                    [dir],
                    |r| r.get::<_, i64>(0),
                )? > 0)
            })
            .await
            .unwrap()
    }
    /// The crewmate's unread wakes' texts, oldest first.
    async fn unread(&self) -> Vec<String> {
        self.fixture
            .db()
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT payload ->> 'text' FROM wakes
                     WHERE target_agent_id = ?1 AND consumed_at IS NULL ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map([CREWMATE], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }
    /// Reads the crewmate's wakes, as a turn would.
    async fn read_wakes(&self) {
        self.fixture
            .db()
            .call(|conn| {
                conn.execute(
                    "UPDATE wakes SET delivered_turn_id = 1, delivered_at = 1, consumed_at = 1
                     WHERE target_agent_id = ?1 AND consumed_at IS NULL",
                    [CREWMATE],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn events(&self, kind: &'static str) -> i64 {
        self.fixture
            .db()
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM events WHERE kind = ?1 AND task_id = 2",
                    [kind],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap()
    }
    fn worktree(&self) -> PathBuf {
        self.svc.deps().data_dir.worktree(task())
    }
    fn branches(&self) -> String {
        git(
            &self.repo,
            &["branch", "--list", "--format=%(refname:short)"],
        )
    }
}
impl Drop for Move {
    fn drop(&mut self) {
        self.scheduler.abort();
    }
}

#[tokio::test]
async fn an_in_place_crewmate_moves_to_another_folder_and_is_told() {
    let m = Move::new().await;
    let moved = m.moved(to(&m.b)).await;
    assert_eq!(
        moved.when,
        MoveTiming::Moved,
        "no runner: the move is made at once"
    );
    assert_eq!(moved.folder, m.b.display().to_string());
    assert!(moved.in_place && moved.branch.is_none());

    let row = m.row().await;
    assert_eq!(row.id, task(), "its name stays");
    assert!(row.in_place);
    assert_eq!(row.delivery, DeliveryMode::Folder);
    assert_eq!(row.status, TaskStatus::Running);
    assert_eq!(m.project_name().await, "b");
    assert_eq!(m.home().await, m.b, "its working folder moved");
    assert!(m.pending().await.is_none());
    assert_eq!(m.events(moving::MOVED_EVENT).await, 1);
    // One message: the move it never heard of is replaced by the news that it moved.
    let unread = m.unread().await;
    assert_eq!(unread.len(), 1, "{unread:?}");
    assert!(
        unread[0].contains(&format!("you now work in {}", m.b.display())),
        "{unread:?}"
    );
    assert!(unread[0].contains("conversation carries on"), "{unread:?}");
    // Its brief names the new folder.
    let brief = std::fs::read_to_string(
        m.svc
            .deps()
            .data_dir
            .crewmate_root(task())
            .join(qm_types::data_dir::BRIEF),
    )
    .unwrap();
    assert!(brief.contains(&m.b.display().to_string()), "{brief}");
}

#[tokio::test]
async fn a_crewmate_moves_into_a_worktree_and_out_and_its_branch_goes_only_without_commits() {
    let m = Move::new().await;
    let base = git(&m.repo, &["rev-parse", "HEAD"]);

    // A plain folder into a repository: a worktree on a new branch from its default branch.
    let moved = m.moved(to(&m.repo)).await;
    assert_eq!(moved.when, MoveTiming::Moved);
    assert!(!moved.in_place);
    let branch = moved.branch.clone().expect("a worktree has a branch");
    let row = m.row().await;
    assert!(!row.in_place);
    assert_eq!(row.branch, branch);
    assert_eq!(
        row.base_commit.as_ref().map(ToString::to_string),
        Some(base.clone())
    );
    assert_ne!(row.delivery, DeliveryMode::Folder);
    assert_eq!(m.home().await, m.worktree());
    assert_eq!(
        git(&m.worktree(), &["rev-parse", "--abbrev-ref", "HEAD"]),
        branch
    );
    assert!(m.worktree().join("README").is_file());
    m.read_wakes().await;

    // Out again, with no commits of its own: the worktree and its branch go.
    let moved = m.moved(to(&m.a)).await;
    assert_eq!(moved.when, MoveTiming::Moved);
    assert!(m.row().await.in_place);
    assert_eq!(m.home().await, m.a);
    assert!(!m.worktree().exists(), "the worktree was removed");
    assert_eq!(m.branches(), "main", "a branch with no commits goes");
    m.read_wakes().await;

    // In again, then a commit of its own: the branch stays when it moves out.
    let moved = m.moved(to(&m.repo)).await;
    let branch = moved.branch.unwrap();
    std::fs::write(m.worktree().join("work.txt"), "work\n").unwrap();
    git(&m.worktree(), &["add", "work.txt"]);
    git(
        &m.worktree(),
        &["-c", "commit.gpgSign=false", "commit", "-m", "work"],
    );
    m.read_wakes().await;
    let moved = m.moved(to(&m.b)).await;
    assert_eq!(moved.when, MoveTiming::Moved);
    assert_eq!(m.home().await, m.b);
    assert!(
        m.branches().contains(&branch),
        "a branch with commits stays"
    );
    let unread = m.unread().await;
    assert!(
        unread
            .iter()
            .any(|t| t.contains(&format!("old branch {branch} stays"))),
        "{unread:?}"
    );
}

#[tokio::test]
async fn a_dirty_worktree_waits_and_naming_where_it_works_calls_the_move_off() {
    let m = Move::new().await;
    m.moved(to(&m.repo)).await;
    m.read_wakes().await;
    let before = m.row().await;
    std::fs::write(m.worktree().join("draft.txt"), "draft\n").unwrap();

    // Uncommitted work: the move waits, and the crewmate is asked to commit or discard it.
    let moved = m.moved(to(&m.a)).await;
    assert_eq!(moved.when, MoveTiming::BetweenTurns);
    assert!(m.pending().await.is_some());
    assert_eq!(m.row().await.branch, before.branch, "nothing moved yet");
    assert_eq!(m.home().await, m.worktree());
    assert!(
        m.worktree().join("draft.txt").is_file(),
        "its work is untouched"
    );
    let unread = m.unread().await;
    assert!(
        unread.iter().any(|t| t.contains("uncommitted changes")),
        "{unread:?}"
    );
    assert!(m.listed(&m.a).await, "where it is going is listed");

    // Naming the worktree's project again calls it off; the unread news goes with it.
    let repo = m.repo.clone();
    let moved = m.moved(to(&repo)).await;
    assert_eq!(moved.when, MoveTiming::CalledOff);
    assert!(m.pending().await.is_none());
    assert!(!m.listed(&m.a).await, "and leaves the list once called off");
    assert!(m.unread().await.is_empty(), "{:?}", m.unread().await);
    assert_eq!(m.home().await, m.worktree());

    // Once it's clean, the move goes through.
    std::fs::remove_file(m.worktree().join("draft.txt")).unwrap();
    assert_eq!(m.moved(to(&m.a)).await.when, MoveTiming::Moved);
    assert_eq!(m.home().await, m.a);
}

#[tokio::test]
async fn a_new_move_replaces_one_that_waits() {
    let m = Move::new().await;
    m.moved(to(&m.repo)).await;
    m.read_wakes().await;
    std::fs::write(m.worktree().join("draft.txt"), "draft\n").unwrap();
    assert_eq!(m.moved(to(&m.a)).await.when, MoveTiming::BetweenTurns);
    assert_eq!(m.moved(to(&m.b)).await.when, MoveTiming::BetweenTurns);
    assert_eq!(m.pending().await.unwrap().home, m.b);
    assert!(
        !m.listed(&m.a).await,
        "the replaced destination isn't listed"
    );
    assert!(m.listed(&m.b).await);
    let unread = m.unread().await;
    assert!(
        !unread
            .iter()
            .any(|t| t.contains(&m.a.display().to_string())),
        "the replaced move's news went: {unread:?}"
    );
}

#[tokio::test]
async fn a_queued_crewmate_moves_in_the_store_alone() {
    let m = Move::new().await;
    m.fixture
        .db()
        .call(|conn| {
            conn.execute("UPDATE tasks SET status = 'queued' WHERE id = 2", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let moved = m.moved(to(&m.repo)).await;
    assert_eq!(moved.when, MoveTiming::Moved);
    let row = m.row().await;
    assert!(!row.in_place);
    assert!(!row.branch.is_empty() && row.base_commit.is_some());
    assert!(!m.worktree().exists(), "preparing makes the worktree");
    assert!(
        m.unread().await.is_empty(),
        "its brief will say where it works"
    );
}

#[tokio::test]
async fn moves_are_refused_where_they_would_lose_or_bend_work() {
    let m = Move::new().await;
    let here = m.a.clone();
    assert!(m.refusal(to(&here)).await.contains("already works there"));
    let mut named = to(&m.b);
    named.base_branch = Some("main".into());
    assert!(m.refusal(named).await.contains("no branches"));

    for (status, words) in [
        ("preparing", "still being prepared"),
        ("in_review", "once that's settled"),
        ("closed", "start a new crewmate there"),
    ] {
        m.fixture
            .db()
            .call(move |conn| {
                conn.execute(
                    "UPDATE tasks SET status = ?1,
                     closed_at = CASE ?1 WHEN 'closed' THEN 1 END,
                     outcome = CASE ?1 WHEN 'closed' THEN 'stopped' END WHERE id = 2",
                    [status],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let reason = m.refusal(to(&m.b)).await;
        assert!(reason.contains(words), "{status}: {reason}");
    }
    assert_eq!(m.home().await, m.a, "a refused move changes nothing");
    assert!(m.pending().await.is_none());

    // Another orchestrator's crewmate isn't the caller's to move.
    let other = Caller {
        agent: AgentId::new(9),
        role: Role::Secondmate,
        ..qm()
    };
    assert!(m.svc.set_task_project(&other, to(&m.b)).await.is_err());
}

#[tokio::test]
async fn a_pushed_branch_stays_with_its_worktree() {
    let m = Move::new().await;
    m.moved(to(&m.repo)).await;
    m.fixture
        .db()
        .call(|conn| {
            conn.execute("UPDATE tasks SET pr_number = 12 WHERE id = 2", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(m.refusal(to(&m.a)).await.contains("pull request #12"));
    assert_eq!(m.home().await, m.worktree());
}

#[tokio::test]
async fn a_waiting_move_is_dropped_when_its_branch_gets_a_pull_request_first() {
    let m = Move::new().await;
    m.moved(to(&m.repo)).await;
    std::fs::write(m.worktree().join("draft.txt"), "draft\n").unwrap();
    assert_eq!(m.moved(to(&m.a)).await.when, MoveTiming::BetweenTurns);
    // Before its next safe point, it commits and opens a pull request.
    std::fs::remove_file(m.worktree().join("draft.txt")).unwrap();
    m.fixture
        .db()
        .call(|conn| {
            conn.execute("UPDATE tasks SET pr_number = 12 WHERE id = 2", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let applied = moving::apply(&m.svc, task(), None).await;
    assert!(
        matches!(&applied, moving::Applied::Failed(why) if why.contains("pull request #12")),
        "{applied:?}"
    );
    assert!(m.pending().await.is_none());
    assert_eq!(
        m.home().await,
        m.worktree(),
        "it stays with its pull request"
    );
    assert!(m.worktree().join("README").is_file());
    assert_eq!(m.events(moving::MOVE_FAILED_EVENT).await, 1);
    // Its supervisor hears why.
    let told: i64 = m
        .fixture
        .db()
        .call(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM wakes WHERE target_agent_id = 1 AND kind = 'workplace_changed'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(told, 1);
}
