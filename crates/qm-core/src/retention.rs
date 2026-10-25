//! What the app keeps on disk for work in progress, and when each piece goes.
//!
//! Plans, evidence, reports and provider-engine transcripts live in the data directory so a restart
//! loses none of them; each crewmate's sit under its task, `artifacts/T-t/C-n/`. Retiring a
//! crewmate retires its files too: each goes once no reader is left. Only these files, which the
//! app made for the crewmate, are ever removed; what a crewmate wrote anywhere else stays.
//!
//! | What | Kept while |
//! |---|---|
//! | `artifacts/T-t/C-n/plan/` | C-n is open, or an open crewmate that can change code builds on C-n and has no copy of the plan yet |
//! | `artifacts/T-t/C-n/evidence/` | C-n is open, or an open crewmate builds on or retries C-n |
//! | `artifacts/T-t/C-n/report/` | C-n is open, or its report's card still waits for you to open or dismiss it |
//! | `artifacts/T-t/C-n/`, `artifacts/T-t/` | while it holds anything |
//! | a link `artifacts/T-t/C-n` or `scratchpad/T-t/C-n`, left when C-n moved to another task | C-n is open |
//! | `scratchpad/T-t/C-n/` (in the scratch root) | C-n is open |
//! | a transcript, `<folder>/transcript/<session>.jsonl` | its owner (the seat, a live secondmate, an open crewmate) is on that session |
//! | a temporary file of an atomic write, a plan copy's staging folder | for [`TEMP_AFTER`] |
//!
//! [`sweep`] applies the table. It runs at startup (before any session opens, so a transcript no
//! one names goes at once and an older layout's files move in), after a
//! task closes, after you open or dismiss a closed task's report ([`after_report_read`]), and
//! every [`SWEEP_EVERY`], which also takes what a retired secondmate or `/clear`
//! left behind. Away from
//! startup a transcript no one names is kept for [`TEMP_AFTER`], so a session that has just
//! started but not yet been recorded is never touched. A damaged transcript that its owner names
//! is kept. Nothing here follows a link: a link is removed itself, and a folder whose ancestors
//! aren't private is left alone. Claude Code's own files are never read or touched.

use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::retention::{Facts, OpenTask, Owner};
use qm_types::clock::Clock;
use qm_types::data_dir::{DataDir, PLANS, TaskPaths, ensure_scratch_folder, remove_path};
use qm_types::fsio::is_temporary;
use qm_types::ids::{TaskId, TaskLabelId};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How long a leftover temporary file, staging folder or unnamed transcript waits.
pub const TEMP_AFTER: Duration = Duration::from_hours(1);
/// How often the sweep runs while the app does.
pub const SWEEP_EVERY: Duration = Duration::from_hours(1);
/// The prefix of a plan copy's staging folder in `artifacts/T-t/C-n/`.
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
    let kinds = crate::reports::api::card_kinds();
    let facts = match db
        .call(move |conn| qm_store::retention::facts(conn, &kinds))
        .await
    {
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

/// How long a report you opened stays, so your browser has loaded it before it goes.
pub const OPENED_REPORT_GRACE: Duration = Duration::from_mins(1);

/// You opened (`opened`) or dismissed a report: once its task is closed, nothing needs the page
/// any more, so a sweep takes it; after [`OPENED_REPORT_GRACE`] if you opened it. Runs in the
/// background.
pub fn after_report_read(db: Db, data: DataDir, clock: Arc<dyn Clock>, opened: bool) {
    tokio::spawn(async move {
        if opened {
            clock.sleep(OPENED_REPORT_GRACE).await;
        }
        sweep(&db, &data, clock.as_ref(), When::Running).await;
    });
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
    if when == When::Startup {
        file_under_tasks(data, facts, &mut tally);
    }
    artifacts(data, facts, now, when, &mut tally);
    legacy_scratch(data, facts, &mut tally);
    scratchpads(data, facts, &mut tally);
    transcripts(data, facts, now, when, &mut tally);
    if tally != Tally::default() {
        tracing::info!(
            removed = tally.removed,
            moved = tally.moved,
            "kept files: removed what finished work no longer needs"
        );
    }
}

/// The open crewmate `id`, if it is one.
fn open_crewmate(facts: &Facts, id: TaskId) -> Option<&OpenTask> {
    facts.open.iter().find(|t| t.id == id)
}

/// Where the files of crewmate `id` are filed now: its task's folders while it is open, else its
/// own number's (only open crewmates' files move; a closed one's stay where they were).
fn filed(data: &DataDir, facts: &Facts, id: TaskId) -> TaskPaths {
    let folders = open_crewmate(facts, id).map_or_else(|| id.own_label(), |t| t.folder_label);
    data.task(id, folders)
}

/// Whether C-`id`'s plan folder is still needed.
fn keep_plan(data: &DataDir, facts: &Facts, id: TaskId) -> bool {
    facts.open.iter().any(|t| {
        t.id == id
            || (t.builds_on == Some(id)
                && !data
                    .task(t.id, t.folder_label)
                    .plan
                    .join("plan.md")
                    .is_file())
    })
}

/// Whether C-`id`'s evidence is still needed.
fn keep_evidence(facts: &Facts, id: TaskId) -> bool {
    facts
        .open
        .iter()
        .any(|t| t.id == id || t.builds_on == Some(id) || t.retry_of == Some(id))
}

/// A crewmate's own folder in a task's folder: named `C-0042`, never the older `T-0042`.
fn crewmate_name(name: &str) -> Option<TaskId> {
    name.starts_with(TaskId::PREFIX)
        .then(|| name.parse().ok())
        .flatten()
}

/// Whether a link is one.
fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether `dir`, named like a task, holds an older version's files of one crewmate rather than a
/// task's crewmates: anything in it but crewmates' folders and links.
fn holds_one_crewmate(dir: &Path) -> bool {
    entries(dir)
        .iter()
        .any(|(name, path)| crewmate_name(name).is_none() && !is_link(path))
}

/// At startup, files from before tasks held crewmates: `artifacts/T-0042/{plan,…}` and
/// `scratchpad/T-0042/` were crewmate 42's own, and it now belongs to task `T-0042`, so they move
/// into `…/T-0042/C-0042/`. An open crewmate's artifacts keep a link at each old path, for the
/// paths it was given.
fn file_under_tasks(data: &DataDir, facts: &Facts, tally: &mut Tally) {
    let dir = data.artifacts_dir();
    if fs::symlink_metadata(&dir).is_ok() && ensure_scratch_folder(&dir).is_ok() {
        for (name, path) in subfolders(&dir) {
            let Ok(label) = name.parse::<TaskLabelId>() else {
                continue;
            };
            if !holds_one_crewmate(&path) {
                continue;
            }
            let id = label.legacy_crewmate();
            let target = data.task(id, label).artifacts;
            let open = open_crewmate(facts, id).is_some();
            for (entry, from) in entries(&path) {
                if crewmate_name(&entry).is_some() || is_link(&from) {
                    continue;
                }
                let to = target.join(&entry);
                let moved = ensure_scratch_folder(&target).and_then(|()| {
                    if open && fs::symlink_metadata(&from).is_ok_and(|m| m.is_dir()) {
                        qm_types::data_dir::migrate_directory(&from, &to)
                    } else {
                        fs::rename(&from, &to)
                    }
                });
                match moved {
                    Ok(()) => tally.moved += 1,
                    Err(e) => tracing::error!(
                        from = %from.display(),
                        error = %e,
                        "an older crewmate's files couldn't be filed under its task; they stay"
                    ),
                }
            }
        }
    }
    let pads = data.scratchpads_dir();
    if fs::symlink_metadata(&pads).is_ok() && ensure_scratch_folder(&pads).is_ok() {
        for (name, path) in subfolders(&pads) {
            let Ok(label) = name.parse::<TaskLabelId>() else {
                continue;
            };
            if !holds_one_crewmate(&path) {
                continue;
            }
            let id = label.legacy_crewmate();
            if open_crewmate(facts, id).is_none() {
                remove(&path, tally);
                continue;
            }
            // Renamed aside, then back in as the crewmate's folder inside its task's.
            let aside = pads.join(format!(".{name}.filing"));
            let target = data.task(id, label).scratchpad;
            let moved = fs::rename(&path, &aside)
                .and_then(|()| ensure_scratch_folder(&path))
                .and_then(|()| fs::rename(&aside, &target));
            match moved {
                Ok(()) => tally.moved += 1,
                Err(e) => tracing::error!(
                    from = %path.display(),
                    error = %e,
                    "an older scratchpad couldn't be filed under its task"
                ),
            }
        }
    }
}

/// `artifacts/T-n/C-n/`: each crewmate's plan, evidence and report go by the table in this
/// module's docs; a link a relabel left goes once its crewmate is closed; a folder goes once
/// empty.
fn artifacts(data: &DataDir, facts: &Facts, now: SystemTime, when: When, tally: &mut Tally) {
    let dir = data.artifacts_dir();
    if !dir.exists() {
        return;
    }
    if let Err(e) = ensure_scratch_folder(&dir) {
        tracing::error!(error = %e, "the artifacts folder isn't private; nothing in it removed");
        return;
    }
    for (name, task_dir) in subfolders(&dir) {
        let Ok(label) = name.parse::<TaskLabelId>() else {
            continue;
        };
        for (entry, path) in entries(&task_dir) {
            let crewmate = crewmate_name(&entry);
            if is_link(&path) {
                // A crewmate's folder moved to another task, or (named `plan`, `evidence`, …) one
                // of an older version's files moved into `C-n/`.
                let owner = crewmate.unwrap_or_else(|| label.legacy_crewmate());
                if open_crewmate(facts, owner).is_none() {
                    remove(&path, tally);
                }
                continue;
            }
            let Some(id) = crewmate else {
                continue;
            };
            let paths = data.task(id, label);
            if keep_plan(data, facts, id) {
                old_temporaries(&paths.plan, now, when, tally);
            } else {
                remove(&paths.plan, tally);
            }
            if !keep_evidence(facts, id) {
                remove(&paths.evidence, tally);
            }
            if open_crewmate(facts, id).is_none() && !facts.unread_reports.contains(&id) {
                remove(&paths.artifacts.join("report"), tally);
            }
            for (staged, staged_path) in entries(&path) {
                if staged.starts_with(PLAN_STAGING) && old_enough(&staged_path, now, when) {
                    remove(&staged_path, tally);
                }
            }
            // Gone once empty; `remove_dir` refuses a folder that still holds anything.
            if fs::remove_dir(&path).is_ok() {
                tally.removed += 1;
            }
        }
        if fs::remove_dir(&task_dir).is_ok() {
            tally.removed += 1;
        }
    }
}

/// `scratchpad/T-n/C-n/`: a crewmate's scratchpad goes with it; a link a relabel left goes once
/// its crewmate is closed; anything else in the scratchpads' folder goes.
fn scratchpads(data: &DataDir, facts: &Facts, tally: &mut Tally) {
    let pads = data.scratchpads_dir();
    if fs::symlink_metadata(&pads).is_err() || ensure_scratch_folder(&pads).is_err() {
        return;
    }
    for (name, task_dir) in entries(&pads) {
        let Some(label) = name
            .parse::<TaskLabelId>()
            .ok()
            .filter(|_| !is_link(&task_dir))
        else {
            remove(&task_dir, tally);
            continue;
        };
        // An older version's scratchpad that startup couldn't file under its task stays while its
        // crewmate is open.
        if holds_one_crewmate(&task_dir) && open_crewmate(facts, label.legacy_crewmate()).is_some()
        {
            continue;
        }
        for (entry, path) in entries(&task_dir) {
            let open = crewmate_name(&entry).is_some_and(|id| open_crewmate(facts, id).is_some());
            if !open {
                remove(&path, tally);
            }
        }
        if fs::remove_dir(&task_dir).is_ok() {
            tally.removed += 1;
        }
    }
}

/// An older layout's plans and evidence, in the scratch root: what is still needed moves into
/// `artifacts/` (a link is left at the old path, for the paths agents were given); the rest goes,
/// and so do the links once their crewmate's files are no longer needed.
fn legacy_scratch(data: &DataDir, facts: &Facts, tally: &mut Tally) {
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
            if !keep(data, facts, id) {
                remove(&path, tally);
            } else if !is_link(&path) {
                let paths = filed(data, facts, id);
                let target = if kind == PLANS {
                    paths.plan
                } else {
                    paths.evidence
                };
                move_in(&path, &target, tally);
            }
        }
    }
    // Evidence once lived in the crewmate's own folder.
    for t in &facts.open {
        let paths = data.task(t.id, t.folder_label);
        let old = paths.root.join(qm_types::data_dir::EVIDENCE);
        if fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir()) {
            move_in(&old, &paths.evidence, tally);
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
    folders.extend(facts.open.iter().map(|t| {
        (
            Owner::Task(t.id),
            data.task(t.id, t.folder_label).transcript,
        )
    }));
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
