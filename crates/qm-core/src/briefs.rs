//! The crewmate's brief, written by the app, and its system prompt.
//!
//! [`write_brief`] gathers the facts the core recorded when the task started (the task row, its
//! project, its supervisor, the user's words as the stored message holds them, and what it builds
//! on), renders them with `qm_prompts::brief::render_brief`, and saves the text as
//! `tasks/C-0042/brief.md` atomically, mode 0600, exactly as the runner then sends it.
//!
//! [`crewmate_system_prompt`] wraps `qm_prompts::brief::crewmate_prompt` for the engine the
//! crewmate runs on: appended to Claude Code's own preset on Claude, the whole prompt elsewhere.

use crate::skills::Gathered;
use qm_engine::SystemPrompt;
use qm_git::{Git, Repo};
use qm_policy::repair::MAX_FAILED_REPAIRS;
use qm_policy::verification;
use qm_prompts::brief::{
    BriefBuildsOn, BriefConstraint, BriefFacts, BriefSupervisor, BriefVerification, BriefWorkspace,
    PromptEngine, crewmate_prompt, render_brief,
};
use qm_store::StoreError;
use qm_store::crew_view::{self, BriefInputs, PriorTask, StoredReport};
use qm_store::db::Db;
use qm_types::data_dir::DataDir;
use qm_types::domain::{DeliveryMode, EngineKind, PlanMode, ReportOutcome, Role, TaskKind};
use qm_types::ids::{CommitSha, PortBlock, TaskId};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The longest builds-on summary, in characters.
pub const SUMMARY_CHARS: usize = 400;

/// The fact shown when what a task builds on isn't a finished plan or a confirmed diagnosis.
pub const NOT_FINISHED: &str = "not a finished plan or confirmed diagnosis";

/// The brief as written: its text (what the runner sends) and where it was saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenBrief {
    pub text: String,
    pub path: PathBuf,
}

/// Why a brief couldn't be written.
#[derive(Debug, thiserror::Error)]
pub enum BriefError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("task {0} doesn't exist")]
    NoTask(TaskId),
    /// The start hasn't recorded everything the brief states yet (its base, its ports).
    #[error("task {task} has no {missing} recorded yet")]
    NotReady { task: TaskId, missing: &'static str },
    #[error("{action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("a file task failed: {0}")]
    Join(String),
}

/// The file, in a task's folder, that holds the whole of the task it builds on.
const BUILDS_ON_FILE: &str = "builds-on.md";

/// Renders the brief of `task` from the store and saves it as `tasks/C-0042/brief.md`
/// (atomically: a temporary file in the task folder, then a rename), mode 0600.
pub async fn write_brief(
    db: &Db,
    data_dir: &DataDir,
    task: TaskId,
) -> Result<WrittenBrief, BriefError> {
    let inputs = db
        .call(move |conn| crew_view::brief_inputs(conn, task))
        .await?
        .ok_or(BriefError::NoTask(task))?;
    let builds_on = builds_on_for(db, data_dir, task, &inputs).await?;
    // The brief shows only the start of a builds-on text; the whole of it is kept in the task's
    // own folder, where the brief points.
    let whole_builds_on = builds_on.as_ref().map(|on| on.body.clone());
    let facts = brief_facts(&inputs, builds_on)?;
    let text = render_brief(&facts);
    let root = data_dir.crewmate_root(task);
    let path = root.join(qm_types::data_dir::BRIEF);
    let written = text.clone();
    let target = path.clone();
    tokio::task::spawn_blocking(move || -> Result<(), BriefError> {
        write_private(&root, &target, &written)?;
        if let Some(whole) = whole_builds_on {
            write_private(&root, &root.join(BUILDS_ON_FILE), &whole)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| BriefError::Join(e.to_string()))??;
    Ok(WrittenBrief { text, path })
}

/// What `task` builds on, as its brief states it: the task named by `builds_on`, or by
/// `retry_of` for a stronger-model retry; `None` when it names neither.
pub async fn builds_on(
    db: &Db,
    data_dir: &DataDir,
    task: TaskId,
) -> Result<Option<BriefBuildsOn>, BriefError> {
    let inputs = db
        .call(move |conn| crew_view::brief_inputs(conn, task))
        .await?
        .ok_or(BriefError::NoTask(task))?;
    builds_on_for(db, data_dir, task, &inputs).await
}

/// The crewmate's system prompt for `engine`: appended to Claude Code's preset on Claude, the
/// whole prompt on the provider and scripted engines (which also list the skills).
pub fn crewmate_system_prompt(
    engine: EngineKind,
    gathered: &Gathered,
    plan: Option<PlanMode>,
) -> SystemPrompt {
    match engine {
        EngineKind::Claude => SystemPrompt::AppendToClaudeCode {
            text: crewmate_prompt(
                PromptEngine::Claude,
                &gathered.files,
                &gathered.skills,
                plan,
            ),
        },
        EngineKind::Provider | EngineKind::Scripted => SystemPrompt::Replace {
            text: crewmate_prompt(
                PromptEngine::Provider,
                &gathered.files,
                &gathered.skills,
                plan,
            ),
        },
    }
}

/// The brief's facts from the store's inputs (pure, so a fixture can pin the golden brief).
pub fn brief_facts(
    inputs: &BriefInputs,
    builds_on: Option<BriefBuildsOn>,
) -> Result<BriefFacts, BriefError> {
    let task = inputs.task;
    let not_ready = |missing| BriefError::NotReady { task, missing };
    let workspace = if inputs.delivery == DeliveryMode::Folder {
        BriefWorkspace::Folder {
            path: inputs.folder.clone(),
            repository: inputs.project_delivery != DeliveryMode::Folder,
        }
    } else {
        BriefWorkspace::Branch {
            branch: inputs.branch.clone().ok_or_else(|| not_ready("branch"))?,
            start_ref: inputs.base_ref.clone().ok_or_else(|| not_ready("base"))?,
            start_commit: inputs
                .base_commit
                .clone()
                .ok_or_else(|| not_ready("base commit"))?,
        }
    };
    let ports = PortBlock {
        base: inputs.port_base.ok_or_else(|| not_ready("port block"))?,
    };
    let supervisor = match (inputs.supervisor.role, inputs.supervisor.secondmate) {
        (Role::Secondmate, Some(id)) => BriefSupervisor::Secondmate(id),
        _ => BriefSupervisor::Quartermaster,
    };
    let verification = match inputs.verification {
        None => BriefVerification::None,
        Some((level, source)) => {
            let (runs, _) = verification::effective(level, inputs.delivery);
            BriefVerification::Level {
                level,
                source,
                ships_as_standard: runs != level,
            }
        }
    };
    let pr_target = (inputs.delivery == DeliveryMode::DirectPr && inputs.kind.can_change_code())
        .then(|| {
            inputs
                .base_branch
                .clone()
                .unwrap_or_else(|| inputs.default_branch.clone())
        });
    Ok(BriefFacts {
        task,
        label: inputs.label,
        kind: inputs.kind,
        supervisor,
        project: inputs.project.clone(),
        delivery: inputs.delivery,
        has_remote: inputs.has_remote,
        verification,
        workspace,
        pr_target,
        ports,
        goal: inputs.goal.clone(),
        user_words: inputs.user_words.clone(),
        instructions: inputs.instructions.clone(),
        constraints: inputs
            .constraints
            .iter()
            .map(|c| BriefConstraint {
                text: c.text.clone(),
                freeze: c.freeze.clone(),
            })
            .collect(),
        tests_required: inputs.tests_required,
        builds_on,
        repair_attempts: MAX_FAILED_REPAIRS,
    })
}

async fn builds_on_for(
    db: &Db,
    data_dir: &DataDir,
    task: TaskId,
    inputs: &BriefInputs,
) -> Result<Option<BriefBuildsOn>, BriefError> {
    let (prior_id, retry) = match (inputs.builds_on, inputs.retry_of) {
        (Some(task), _) => (task, false),
        (None, Some(task)) => (task, true),
        (None, None) => return Ok(None),
    };
    let Some(mut prior) = db
        .call(move |conn| crew_view::prior_task(conn, prior_id))
        .await?
    else {
        return Ok(None);
    };
    // A review is about its immutable start SHA, not a builder head that can advance while
    // routing, preparing or writing this brief. Do not fall back to a newer clear report.
    if inputs.kind == TaskKind::Review && !retry {
        prior.head_commit.clone_from(&inputs.base_commit);
        if let Some(report) = prior.last_clear.as_mut() {
            report.head_commit.clone_from(&inputs.base_commit);
        }
    }
    // The answer is the report's own text, kept with the task; there is no file to read.
    let answer = prior.last_clear.as_ref().and_then(|r| r.answer.clone());
    let changed = if inputs.kind == TaskKind::Review && !retry {
        review_files(&prior).await
    } else {
        Vec::new()
    };
    let answer_path = format!("tasks/{task}/{BUILDS_ON_FILE}");
    let mut facts = prior_facts(
        inputs.kind,
        retry,
        &prior,
        answer.as_deref(),
        &changed,
        answer_path,
    );
    if inputs.kind == TaskKind::Review && !retry {
        crate::verification::review::append_brief_extras(db, data_dir, prior_id, &mut facts.body)
            .await;
    }
    Ok(Some(facts))
}

/// The head of the work a prior task did: its recorded head, else its last clear report's.
fn prior_head(prior: &PriorTask) -> Option<CommitSha> {
    prior.head_commit.clone().or_else(|| {
        prior
            .last_clear
            .as_ref()
            .and_then(|r| r.head_commit.clone())
    })
}

/// Every file the work under review changed, from where its branch started to its head
/// (`Repo::changed_files`). Without a recorded base or head, or if git can't say, the list is
/// left out (and the failure logged) rather than holding up the review.
async fn review_files(prior: &PriorTask) -> Vec<PathBuf> {
    let (Some(base), Some(head)) = (prior.base_commit.clone(), prior_head(prior)) else {
        return Vec::new();
    };
    let repo_path = PathBuf::from(&prior.project_path);
    let task = prior.task;
    let listed = tokio::task::spawn_blocking(move || {
        Repo::open(Git::new(), &repo_path)?.changed_files(&base, &head)
    })
    .await;
    match listed {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            tracing::warn!(%task, error = %e, "couldn't list the files under review");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(%task, error = %e, "listing the files under review failed");
            Vec::new()
        }
    }
}

/// What the brief says about the task it builds on.
fn prior_facts(
    kind: TaskKind,
    retry: bool,
    prior: &PriorTask,
    answer: Option<&str>,
    changed: &[PathBuf],
    answer_path: String,
) -> BriefBuildsOn {
    let clear = prior.last_clear.as_ref();
    let head = prior_head(prior);
    let fact = if retry {
        "the failed attempt this task retries".to_owned()
    } else if kind == TaskKind::Review {
        match &head {
            Some(head) => format!("the work under review at {head}"),
            None => match &prior.branch {
                Some(branch) => format!("the work under review on {branch}"),
                None => "the work under review".to_owned(),
            },
        }
    } else {
        let done = clear.is_some_and(|r| r.outcome == Some(ReportOutcome::Done));
        match prior.kind {
            TaskKind::Plan if done => "finished plan".to_owned(),
            TaskKind::Reproduce => match clear.and_then(|r| r.reproduced) {
                Some(true) => "reproduced: yes".to_owned(),
                Some(false) => "reproduced: no".to_owned(),
                None => NOT_FINISHED.to_owned(),
            },
            TaskKind::Research if done && clear.is_some_and(|r| r.diagnosis) => {
                "confirmed diagnosis".to_owned()
            }
            _ => NOT_FINISHED.to_owned(),
        }
    };

    let report = clear.or(prior.last_any.as_ref());
    let summary = answer
        .map(first_paragraph)
        .filter(|s| !s.is_empty())
        .or_else(|| report.and_then(|r| r.headings.get("intent").map(|s| s.trim().to_owned())))
        .map(|s| cut_chars(&s, SUMMARY_CHARS))
        .unwrap_or_default();

    let body = if head.is_none() && answer.is_some() {
        answer.unwrap_or_default().to_owned()
    } else {
        let mut body = match (&prior.branch, &head) {
            (Some(branch), Some(head)) if kind == TaskKind::Review => {
                format!("Branch {branch} @ {head}\n")
            }
            (Some(branch), Some(head)) => format!("Branch {branch} @ {}\n", head.short()),
            (Some(branch), None) => format!("Branch {branch}\n"),
            (None, _) => String::new(),
        };
        if let Some(report) = report {
            body.push_str(&report_text(report));
        }
        if !changed.is_empty() {
            body.push_str("Changed files:\n");
            for file in changed {
                body.push_str("- ");
                body.push_str(&file.to_string_lossy());
                body.push('\n');
            }
        }
        body
    };
    BriefBuildsOn {
        task: prior.task,
        kind: prior.kind,
        fact,
        summary,
        body,
        answer_path,
    }
}

/// A report's five headings, one per paragraph, or its kept final text.
fn report_text(report: &StoredReport) -> String {
    const HEADINGS: [(&str, &str); 5] = [
        ("intent", "Intent"),
        ("what_changed", "What changed"),
        ("verified", "Verified"),
        ("risk", "Risk"),
        ("not_done", "Not done"),
    ];
    let mut out = String::new();
    for (key, title) in HEADINGS {
        if let Some(text) = report.headings.get(key).map(|s| s.trim())
            && !text.is_empty()
        {
            out.push_str(title);
            out.push_str(": ");
            out.push_str(text);
            out.push('\n');
        }
    }
    if out.is_empty()
        && let Some(text) = report.final_text.as_deref().map(str::trim)
    {
        out.push_str(text);
        out.push('\n');
    }
    out
}

/// The answer's first paragraph, after a leading `reproduced:` or `verdict:` line.
fn first_paragraph(answer: &str) -> String {
    let mut lines = answer.lines().map(str::trim).skip_while(|l| l.is_empty());
    let mut paragraph: Vec<&str> = Vec::new();
    if let Some(first) = lines.next() {
        let lower = first.to_ascii_lowercase();
        if !(lower.starts_with("reproduced:") || lower.starts_with("verdict:")) {
            paragraph.push(first);
        }
    }
    for line in lines {
        if line.is_empty() {
            if paragraph.is_empty() {
                continue;
            }
            break;
        }
        paragraph.push(line);
    }
    paragraph.join(" ")
}

/// `text` cut to `max` characters, with `…` when it was cut.
fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// Writes `text` to `path` through a temporary file in `dir` (created 0700 if missing), mode
/// 0600, then renames it into place.
fn write_private(dir: &Path, path: &Path, text: &str) -> Result<(), BriefError> {
    let io = |action, path: &Path| {
        let path = path.to_owned();
        move |source| BriefError::Io {
            action,
            path,
            source,
        }
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io("can't create", dir))?;
    let tmp = dir.join(".brief.md.tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io("can't create", &tmp))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io("can't write", &tmp))?;
    drop(file);
    fs::rename(&tmp, path).map_err(io("can't save", path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_folder_task_loads_and_omits_branch_from_brief() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open fixture");
        qm_store::schema::migrate(&mut conn).expect("migrate");
        conn.execute_batch(
            "INSERT INTO agents (id, role, home, status, created_at)
             VALUES (1, 'quartermaster', '/fixture/seat', 'idle', 0);
             INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                                   merge_method, created_at)
             VALUES (1, 'personal', '/fixture/personal', 'workspace', '', 'folder', 'squash', 0);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                                delivery_mode, branch, status, created_at, seq)
             VALUES (1, 1, 1, 'fix', 'Folder task', 'Prior folder work',
                     'folder', NULL, 'reported', 0, 1),
                    (2, 1, 1, 'fix', 'Git task', 'Prior git work',
                     'branch_only', 'qm/task-0002', 'reported', 1, 2);",
        )
        .expect("seed prior tasks");

        let folder = crew_view::prior_task(&conn, "T-0001".parse().expect("task ID"))
            .expect("read NULL branch")
            .expect("folder task");
        assert_eq!(folder.branch, None);
        let facts = prior_facts(
            TaskKind::Review,
            false,
            &folder,
            None,
            &[],
            "tasks/T-0001/answer.md".to_owned(),
        );
        assert_eq!(facts.fact, "the work under review");
        assert!(facts.body.is_empty(), "no invented branch line");
        assert_eq!(facts.answer_path, "tasks/T-0001/answer.md");
        let retry = prior_facts(
            TaskKind::Fix,
            true,
            &folder,
            None,
            &[],
            "tasks/T-0001/answer.md".to_owned(),
        );
        assert_eq!(retry.fact, "the failed attempt this task retries");
        assert_eq!(retry.body, "");

        let git = crew_view::prior_task(&conn, "T-0002".parse().expect("task ID"))
            .expect("read git branch")
            .expect("git task");
        assert_eq!(git.branch.as_deref(), Some("qm/task-0002"));
        let facts = prior_facts(
            TaskKind::Review,
            false,
            &git,
            None,
            &[],
            "tasks/T-0002/answer.md".to_owned(),
        );
        assert_eq!(facts.fact, "the work under review on qm/task-0002");
        assert_eq!(facts.body, "Branch qm/task-0002\n");
    }
}
