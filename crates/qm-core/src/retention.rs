//! What the app keeps on disk for work in progress, and when each piece goes.
//!
//! Plans, evidence, reports and provider-engine transcripts live in the data directory so a restart
//! loses none of them. Reports are kept until the task is forgotten, even after it closes; they
//! must then go because task IDs can be reused. The other files go once no reader is left:
//!
//! | What | Kept while |
//! |---|---|
//! | `artifacts/T-n/plan/` | T-n is open, or an open task that can change code builds on T-n and has no copy of the plan yet |
//! | `artifacts/T-n/evidence/` | T-n is open, or an open task builds on or retries T-n |
//! | `artifacts/T-n/report/` | T-n's row still exists (until forgotten, not just closed) |
//! | `artifacts/T-n/` | while it holds anything |
//! | a transcript, `<folder>/transcript/<session>.jsonl` | its owner (the seat, a live secondmate, an open task) is on that session |
//! | a temporary file of an atomic write, a plan copy's staging folder | for [`TEMP_AFTER`] |
//!
//! [`sweep`] applies the table. It runs at startup (before any session opens, so a transcript no
//! one names goes at once and an older layout's scratch plans and evidence move in), after a
//! task closes, and every [`SWEEP_EVERY`], which also takes what a retired secondmate or `/clear`
//! left behind. Away from
//! startup a transcript no one names is kept for [`TEMP_AFTER`], so a session that has just
//! started but not yet been recorded is never touched. A damaged transcript that its owner names
//! is kept. Nothing here follows a link: a link is removed itself, and a folder whose ancestors
//! aren't private is left alone. Claude Code's own files are never read or touched.

use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::retention::{Facts, Owner};
use qm_types::clock::Clock;
use qm_types::data_dir::{DataDir, PLANS, SCRATCHPAD, ensure_scratch_folder, remove_path};
use qm_types::fsio::is_temporary;
use qm_types::ids::TaskId;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How long a leftover temporary file, staging folder or unnamed transcript waits.
pub const TEMP_AFTER: Duration = Duration::from_hours(1);
/// How often the sweep runs while the app does.
pub const SWEEP_EVERY: Duration = Duration::from_hours(1);
/// The prefix of a plan copy's staging folder in `artifacts/T-n/`.
pub const PLAN_STAGING: &str = ".plan.tmp-";

/// When the sweep runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// At startup: no session is open yet.
    Startup,
    /// While the app runs.
    Running,
}

/// Reads the facts, then applies the table (in this module's docs) once. A failure is logged.
pub async fn sweep(db: &Db, data: &DataDir, clock: &dyn Clock, when: When) {
    let facts = match db.call(|conn| qm_store::retention::facts(conn)).await {
        Ok(facts) => facts,
        Err(e) => {
            log_failure(&e);
            return;
        }
    };
    let data = data.clone();
    let now = SystemTime::from(clock.now());
    if let Err(e) = tokio::task::spawn_blocking(move || apply(&data, &facts, now, when)).await {
        tracing::warn!(error = %e, "the retention sweep stopped");
    }
}

fn log_failure(e: &StoreError) {
    tracing::warn!(error = %e, "couldn't read what the kept files belong to; nothing removed");
}

/// Runs [`sweep`] every [`SWEEP_EVERY`], for as long as the runtime runs.
pub fn spawn_sweeper(db: Db, data: DataDir, clock: Arc<dyn Clock>) {
    tokio::spawn(async move {
        loop {
            clock.sleep(SWEEP_EVERY).await;
            sweep(&db, &data, clock.as_ref(), When::Running).await;
        }
    });
}

/// What one sweep removed and moved, for its log line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    removed: usize,
    moved: usize,
}

/// The blocking half of [`sweep`].
fn apply(data: &DataDir, facts: &Facts, now: SystemTime, when: When) {
    let mut tally = Tally::default();
    artifacts(data, facts, now, when, &mut tally);
    legacy_scratch(data, facts, &mut tally);
    transcripts(data, facts, now, when, &mut tally);
    if tally != Tally::default() {
        tracing::info!(
            removed = tally.removed,
            moved = tally.moved,
            "kept files: removed what finished work no longer needs"
        );
    }
}

/// Whether T-`id`'s plan folder is still needed.
fn keep_plan(data: &DataDir, facts: &Facts, id: TaskId) -> bool {
    facts.open.iter().any(|t| {
        t.id == id || (t.builds_on == Some(id) && !data.task(t.id).plan.join("plan.md").is_file())
    })
}

/// Whether T-`id`'s evidence is still needed.
fn keep_evidence(facts: &Facts, id: TaskId) -> bool {
    facts
        .open
        .iter()
        .any(|t| t.id == id || t.builds_on == Some(id) || t.retry_of == Some(id))
}

fn artifacts(data: &DataDir, facts: &Facts, now: SystemTime, when: When, tally: &mut Tally) {
    let dir = data.artifacts_dir();
    if !dir.exists() {
        return;
    }
    if let Err(e) = ensure_scratch_folder(&dir) {
        tracing::error!(error = %e, "the artifacts folder isn't private; nothing in it removed");
        return;
    }
    for (name, path) in subfolders(&dir) {
        let Ok(id) = name.parse::<TaskId>() else {
            continue;
        };
        let paths = data.task(id);
        if keep_plan(data, facts, id) {
            old_temporaries(&paths.plan, now, when, tally);
        } else {
            remove(&paths.plan, tally);
        }
        if !keep_evidence(facts, id) {
            remove(&paths.evidence, tally);
        }
        if !facts.retained_tasks.contains(&id) {
            remove(&paths.artifacts.join("report"), tally);
        }
        for (entry, entry_path) in entries(&path) {
            if entry.starts_with(PLAN_STAGING) && old_enough(&entry_path, now, when) {
                remove(&entry_path, tally);
            }
        }
        // Gone once empty; `remove_dir` refuses a folder that still holds anything.
        if fs::remove_dir(&path).is_ok() {
            tally.removed += 1;
        }
    }
}

/// An older layout's plans and evidence, in the scratch root: what is still needed moves into
/// `artifacts/` (a link is left at the old path, for the paths agents were given); the rest goes,
/// and so do the links once their task's files are no longer needed. A scratchpad goes with its
/// task.
fn legacy_scratch(data: &DataDir, facts: &Facts, tally: &mut Tally) {
    let is_open = |id: TaskId| facts.open.iter().any(|t| t.id == id);
    let kinds: [(&str, KeepRule); 2] = [
        (PLANS, keep_plan),
        (qm_types::data_dir::EVIDENCE, |_, facts, id| {
            keep_evidence(facts, id)
        }),
    ];
    for (kind, keep) in kinds {
        let dir = data.scratch().join(kind);
        if fs::symlink_metadata(&dir).is_err() {
            continue;
        }
        if let Err(e) = ensure_scratch_folder(&dir) {
            tracing::error!(error = %e, kind, "an old scratch folder isn't private; it stays");
            continue;
        }
        for (name, path) in entries(&dir) {
            let Ok(id) = name.parse::<TaskId>() else {
                continue;
            };
            let link = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            if !keep(data, facts, id) {
                remove(&path, tally);
            } else if !link {
                let paths = data.task(id);
                let target = if kind == PLANS {
                    paths.plan
                } else {
                    paths.evidence
                };
                move_in(&path, &target, tally);
            }
        }
    }
    // Evidence once lived in the task's own folder.
    for id in facts.open.iter().map(|t| t.id) {
        let paths = data.task(id);
        let old = paths.root.join(qm_types::data_dir::EVIDENCE);
        if fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir()) {
            move_in(&old, &paths.evidence, tally);
        }
    }
    let pads = data.scratch().join(SCRATCHPAD);
    if fs::symlink_metadata(&pads).is_ok() && ensure_scratch_folder(&pads).is_ok() {
        for (name, path) in entries(&pads) {
            if !name.parse::<TaskId>().is_ok_and(is_open) {
                remove(&path, tally);
            }
        }
    }
}

/// Whether a task's files of one kind are still needed.
type KeepRule = fn(&DataDir, &Facts, TaskId) -> bool;

fn move_in(old: &Path, target: &Path, tally: &mut Tally) {
    if let Some(parent) = target.parent()
        && let Err(e) = ensure_scratch_folder(parent)
    {
        tracing::error!(error = %e, "a task's folder isn't private; its old files stay");
        return;
    }
    match qm_types::data_dir::migrate_directory(old, target) {
        Ok(()) => tally.moved += 1,
        Err(e) => tracing::error!(
            from = %old.display(),
            error = %e,
            "old task files preserved after migration failure"
        ),
    }
}

fn transcripts(data: &DataDir, facts: &Facts, now: SystemTime, when: When, tally: &mut Tally) {
    let mut folders: Vec<(Owner, PathBuf)> = vec![(Owner::Seat, data.seat().transcript)];
    folders.extend(facts.sessions.iter().filter_map(|s| match s.owner {
        Owner::Secondmate(id) => Some((s.owner, data.secondmate(id).transcript)),
        _ => None,
    }));
    folders.extend(
        facts
            .open
            .iter()
            .map(|t| (Owner::Task(t.id), data.task(t.id).transcript)),
    );
    folders.sort();
    folders.dedup();
    for (owner, dir) in folders {
        if fs::symlink_metadata(&dir).is_err() {
            continue;
        }
        if let Err(e) = ensure_scratch_folder(&dir) {
            tracing::error!(error = %e, "a transcript folder isn't private; nothing in it removed");
            continue;
        }
        let named: Vec<String> = facts
            .sessions
            .iter()
            .filter(|s| s.owner == owner)
            .filter_map(|s| s.session_ref.as_ref())
            .map(|r| format!("{}.jsonl", r.as_str()))
            .collect();
        for (name, path) in entries(&dir) {
            if named.contains(&name) {
                continue;
            }
            if (is_temporary(&name) || path.extension().is_some_and(|e| e == "jsonl"))
                && old_enough(&path, now, when)
            {
                remove(&path, tally);
            }
        }
    }
}

/// Removes a temporary file in `dir` left by an interrupted atomic write.
fn old_temporaries(dir: &Path, now: SystemTime, when: When, tally: &mut Tally) {
    for (name, path) in entries(dir) {
        if is_temporary(&name) && old_enough(&path, now, when) {
            remove(&path, tally);
        }
    }
}

/// Whether `path` may go: at startup always; otherwise once it is [`TEMP_AFTER`] old.
fn old_enough(path: &Path, now: SystemTime, when: When) -> bool {
    when == When::Startup
        || fs::symlink_metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age >= TEMP_AFTER))
}

fn remove(path: &Path, tally: &mut Tally) {
    if fs::symlink_metadata(path).is_err() {
        return;
    }
    match remove_path(path) {
        Ok(()) => tally.removed += 1,
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "couldn't remove a kept file"),
    }
}

/// The names and paths of the entries directly in `dir`; none if it can't be read.
fn entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect()
}

/// The plain folders directly in `dir` (never a link).
fn subfolders(dir: &Path) -> Vec<(String, PathBuf)> {
    entries(dir)
        .into_iter()
        .filter(|(_, p)| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect()
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
