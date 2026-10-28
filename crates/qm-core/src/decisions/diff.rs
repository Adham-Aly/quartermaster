//! The Diff screen's content.
//!
//! The approval card's Diff button sends `Command::ShowDiff { held: Some(id) }`; [`DiffPart`]
//! loads the held merge's diff ([`DecisionService::load_diff`]) and puts it in every snapshot's
//! `diff` until `ShowDiff { held: None }` closes it. Only merges (and held pushes) have a diff:
//! the stat and the patch from where the branch left its base to the approved head, at most
//! [`DIFF_MAX_LINES`] lines. The core parses changed files, metadata and numbered text hunks for
//! split rendering; the original toned lines remain available for old clients and read errors.
//!
//! Reading the diff is git's: [`DiffReader`] is the seam, and [`GitDiffReader`] reads it with
//! `qm_git`'s `Repo::diff_stat` and `Repo::diff_text`; [`NoDiffReader`] answers that the diff can't
//! be read, for a build without git.

use super::DecisionService;
use crate::api::{ApiPart, Handled};
use qm_store::StoreError;
use qm_store::held::{HeldPayload, MergePayload, PushPayload, load_held};
use qm_store::projects;
use qm_store::tasks as task_store;
use qm_types::BoxFuture;
use qm_types::api::{Command, DiffView, Snapshot, Styled, Tone};
use qm_types::ids::{CommitSha, HeldActionId};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

mod patch;
pub use patch::parse_patch;

/// The most patch lines the Diff screen carries.
pub const DIFF_MAX_LINES: usize = 20_000;

/// A diff as git gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawDiff {
    pub insertions: u64,
    pub deletions: u64,
    pub files: u64,
    /// The patch, line by line, at most the cap.
    pub lines: Vec<String>,
    /// The patch had more lines than the cap.
    pub truncated: bool,
}

/// Reads the diff between where `head`'s branch left `base` and `head`, in the checkout at
/// `repo` (blocking: called on a blocking thread).
pub trait DiffReader: Send + Sync + fmt::Debug {
    fn read(
        &self,
        repo: &Path,
        base: &CommitSha,
        head: &CommitSha,
        max_lines: usize,
    ) -> Result<RawDiff, String>;
}

/// The diff from git, in the project's checkout.
#[derive(Clone, Debug, Default)]
pub struct GitDiffReader {
    git: qm_git::Git,
}

impl GitDiffReader {
    pub fn new(git: qm_git::Git) -> Self {
        Self { git }
    }
}

impl DiffReader for GitDiffReader {
    fn read(
        &self,
        repo: &Path,
        base: &CommitSha,
        head: &CommitSha,
        max_lines: usize,
    ) -> Result<RawDiff, String> {
        let repo = qm_git::Repo::open(self.git.clone(), repo).map_err(|e| e.to_string())?;
        let stat = repo.diff_stat(base, head).map_err(|e| e.to_string())?;
        let (text, truncated) = repo
            .diff_text(base, head, max_lines)
            .map_err(|e| e.to_string())?;
        Ok(RawDiff {
            insertions: stat.additions,
            deletions: stat.deletions,
            files: u64::from(stat.files),
            lines: text.lines().map(str::to_owned).collect(),
            truncated,
        })
    }
}

/// No diff reader wired yet.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDiffReader;

impl DiffReader for NoDiffReader {
    fn read(
        &self,
        repo: &Path,
        base: &CommitSha,
        head: &CommitSha,
        max_lines: usize,
    ) -> Result<RawDiff, String> {
        let _ = (repo, base, head, max_lines);
        Err("The diff can't be read in this build yet.".to_owned())
    }
}

/// The tone of one patch line.
pub fn tone_of(line: &str) -> Tone {
    const HEADERS: &[&str] = &["+++", "---", "@@", "diff --git", "index "];
    if HEADERS.iter().any(|h| line.starts_with(h)) {
        Tone::Accent
    } else if line.starts_with('+') {
        Tone::Ok
    } else if line.starts_with('-') {
        Tone::Error
    } else {
        Tone::Text
    }
}

/// "+214 −37 · 12 files".
pub fn stat_line(raw: &RawDiff) -> String {
    let files = if raw.files == 1 { "file" } else { "files" };
    format!(
        "+{} −{} · {} {files}",
        raw.insertions, raw.deletions, raw.files
    )
}

fn fill_view(view: &mut DiffView, raw: RawDiff) {
    view.stat = stat_line(&raw);
    let (files, incomplete) = patch::parse_with_status(&raw.lines);
    view.files = files;
    view.truncated = raw.truncated || incomplete;
    view.lines = raw
        .lines
        .into_iter()
        .map(|line| {
            let tone = tone_of(&line);
            Styled::new(line, tone)
        })
        .collect();
}

/// What a held action's diff is read from: the checkout, the base and the approved head.
struct Target {
    title: String,
    repo: PathBuf,
    base: CommitSha,
    head: CommitSha,
}

fn target(conn: &rusqlite::Connection, held: HeldActionId) -> Result<Option<Target>, StoreError> {
    let row = load_held(conn, held)?;
    let (task, head) = match &row.payload {
        HeldPayload::Merge(
            MergePayload::PullRequest { task, head, .. } | MergePayload::InPlace { task, head, .. },
        )
        | HeldPayload::Push(PushPayload { task, head, .. }) => (*task, head.clone()),
        _ => return Ok(None),
    };
    let Some(task_row) = task_store::load(conn, task)? else {
        return Ok(None);
    };
    let Some(project) = projects::load(conn, task_row.project)? else {
        return Ok(None);
    };
    // The diff runs from where the task's branch started to the approved head.
    let Some(base) = task_row.base_commit else {
        return Ok(None);
    };
    Ok(Some(Target {
        title: task_row.title,
        repo: project.path,
        base,
        head,
    }))
}

impl DecisionService {
    /// The Diff screen's content for `held`, or `None` when it has no diff.
    pub async fn load_diff(&self, held: HeldActionId) -> Option<DiffView> {
        let target = match self.db().call(move |conn| target(conn, held)).await {
            Ok(Some(t)) => t,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(error = %e, %held, "couldn't read the held action for its diff");
                return None;
            }
        };
        let reader = std::sync::Arc::clone(self.diff_reader());
        let Target {
            title,
            repo,
            base,
            head,
        } = target;
        let mut view = DiffView::new(held, title);
        let read =
            tokio::task::spawn_blocking(move || reader.read(&repo, &base, &head, DIFF_MAX_LINES))
                .await;
        match read {
            Ok(Ok(raw)) => fill_view(&mut view, raw),
            Ok(Err(why)) => view.lines = vec![Styled::new(why, Tone::Warn)],
            Err(e) => view.lines = vec![Styled::new(e.to_string(), Tone::Error)],
        }
        Some(view)
    }
}

/// The API's end of the Diff screen: `ShowDiff`, and `Snapshot.diff`.
#[derive(Debug)]
pub struct DiffPart {
    decisions: DecisionService,
    open: Mutex<Option<DiffView>>,
}

impl DiffPart {
    pub fn new(decisions: DecisionService) -> Self {
        Self {
            decisions,
            open: Mutex::new(None),
        }
    }
}

impl ApiPart for DiffPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        let Command::ShowDiff { held } = command else {
            return None;
        };
        let held = *held;
        Some(Box::pin(async move {
            let view = match held {
                Some(held) => self.decisions.load_diff(held).await,
                None => None,
            };
            *self.open.lock().unwrap_or_else(PoisonError::into_inner) = view;
            Handled::Changed
        }))
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            snapshot
                .diff
                .clone_from(&self.open.lock().unwrap_or_else(PoisonError::into_inner));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_store::held::{NewHeld, insert_held};
    use qm_types::ids::{AgentId, TaskId};

    #[test]
    fn diff_projection_keeps_authored_ids_patch_paths_and_pinned_target() {
        let mut conn = qm_store::connection::open_memory().unwrap();
        qm_store::schema::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
            INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                merge_method, created_at, seq) VALUES
            (1, 'app', '/work/T-0042/C-0042', 'linked', 'main', 'branch_only', 'squash', 0, 1);
            INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, delivery_mode,
                status, created_at, seq, base_commit, branch) VALUES
            (42, 1, 1, 'fix', 'authored T-0042 and C-0047', 'g', 'branch_only',
                'settling', 0, 1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'work/C-0042');",
        )
        .unwrap();
        let task = TaskId::new(42).unwrap();
        let head: CommitSha = "b".repeat(40).parse().unwrap();
        let payload = HeldPayload::Push(PushPayload {
            task,
            head: head.clone(),
            branch: "work/C-0042".into(),
            remote: "origin".into(),
            expected_remote: None,
        });
        let held = insert_held(
            &conn,
            &NewHeld {
                payload: payload.clone(),
                choices: vec![],
                title: "authored T-0042 and C-0047".into(),
                task: Some(task),
                asked_by: Some(AgentId::new(1)),
                at: jiff::Timestamp::UNIX_EPOCH,
            },
        )
        .unwrap();
        let target = target(&conn, held).unwrap().unwrap();
        assert_eq!(target.title, "authored T-0042 and C-0047");
        assert_eq!(target.repo, PathBuf::from("/work/T-0042/C-0042"));
        assert_eq!(target.base.as_str(), "a".repeat(40));
        assert_eq!(target.head, head);
        assert_eq!(load_held(&conn, held).unwrap().payload, payload);
        let lines: Vec<String> = [
            "diff --git a/T-0042/C-0042.rs b/T-0042/C-0042.rs",
            "--- a/T-0042/C-0042.rs",
            "+++ b/T-0042/C-0042.rs",
            "@@ -1 +1 @@",
            "-let id = \"T-0042\";",
            "+let id = \"C-0047\";",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut view = DiffView::new(held, target.title);
        fill_view(
            &mut view,
            RawDiff {
                lines: lines.clone(),
                ..RawDiff::default()
            },
        );
        assert_eq!(view.held, held);
        assert_eq!(
            view.lines
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>(),
            lines
        );
    }
}
