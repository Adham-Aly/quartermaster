//! `task_status` and `find_tasks`: reading tasks. Read only: any supervisor may look.

use super::{ProjectRow, TaskService, active_projects, cut_chars, failed, load_project, refuse};
use qm_store::reports::{self, ReportRow};
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::{FindTasksArgs, FoundTasks, HistoryEntry, TaskDetail, TaskSummary};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{ReportSource, TaskStatus};
use qm_types::ids::{ProjectId, TaskId, TaskLabelId};
use std::collections::HashMap;
use std::fmt::Write as _;

/// The most tasks `find_tasks` lists per project.
pub const PER_PROJECT: usize = 12;
/// The most tasks `find_tasks` lists in all.
pub const IN_ALL: usize = 36;
/// How much of a report a history line shows.
const REPORT_LINE_CHARS: usize = 600;

/// A task's one-line state, as lists show it.
pub fn state_line(row: &TaskRow) -> String {
    match row.status {
        TaskStatus::Queued => "queued: waiting for a model or a blocker".to_owned(),
        TaskStatus::Waiting => match &row.waiting_on {
            Some(w) if w.get("task").is_some() => format!(
                "waiting on {}",
                w.get("task").and_then(|t| t.as_str()).unwrap_or("a task")
            ),
            Some(w) if w.get("external").is_some() => format!(
                "waiting on {}",
                w.get("external")
                    .and_then(|t| t.as_str())
                    .unwrap_or("something outside")
            ),
            _ => "waiting for its supervisor's decision".to_owned(),
        },
        TaskStatus::Closed => match row.outcome {
            Some(o) => format!("closed: {o}"),
            None => "closed".to_owned(),
        },
        s => match &row.head_commit {
            Some(head) => format!("{s} @{}", head.short()),
            None => s.to_string(),
        },
    }
}

fn summary(row: &TaskRow, project: &str) -> TaskSummary {
    TaskSummary {
        task: row.id,
        label: row.label,
        project: project.to_owned(),
        kind: row.kind,
        title: row.title.clone(),
        status: row.status,
        line: state_line(row),
    }
}

fn report_line(r: &ReportRow) -> String {
    let mut text = match (r.source, r.outcome) {
        (ReportSource::ReportCall, Some(o)) => format!("Report ({o}):"),
        _ => "Final text, kept as its report:".to_owned(),
    };
    if let Some(map) = r.headings.as_object() {
        for (key, label) in [
            ("intent", "Intent"),
            ("what_changed", "What changed"),
            ("verified", "Verified"),
            ("risk", "Risk"),
            ("not_done", "Not done"),
        ] {
            if let Some(v) = map.get(key).and_then(|v| v.as_str()) {
                let _ = write!(text, " {label}: {v}");
            }
        }
    }
    if let Some(answer) = &r.answer {
        let _ = write!(text, " Answer: {answer}");
    }
    if let Some(final_text) = &r.final_text {
        text.push(' ');
        text.push_str(final_text);
    }
    if let Some(blockers) = r.blockers.as_array().filter(|b| !b.is_empty()) {
        let list: Vec<&str> = blockers.iter().filter_map(|b| b.as_str()).collect();
        let _ = write!(text, " Blockers: {}", list.join("; "));
    }
    cut_chars(&text, REPORT_LINE_CHARS, "…")
}

/// `task_status`.
pub(crate) async fn task_detail(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
) -> Result<TaskDetail, ToolFailure> {
    let reader = *caller;
    let (row, project, events, reports) = svc
        .db()
        .call(move |conn| {
            let Some(row) = task_store::load(conn, task)? else {
                return Ok(None);
            };
            // A secondmate (or its crewmate) reads its own tasks and its scope's.
            if let Err(r) = crate::secondmates::scope::may_read_task(conn, &reader, task)? {
                return Ok(Some(Err(r)));
            }
            let project = load_project(conn, row.project)?;
            let events = task_store::history(conn, task)?;
            let reports = reports::for_task(conn, task)?;
            Ok(Some(Ok((row, project, events, reports))))
        })
        .await
        .map_err(|e| failed(&e))?
        .ok_or_else(|| refuse(format!(
            "{task}'s details are unavailable: it either closed and was forgotten or doesn't exist; use find_tasks for tasks still kept (numbers can be reused after restart)"
        )))??;
    let mut history: Vec<HistoryEntry> = events
        .iter()
        .filter(|e| e.kind != super::runner::TOOL_EVENT)
        .map(|e| HistoryEntry {
            at: e.at,
            text: e.summary.clone(),
        })
        .collect();
    history.extend(reports.iter().map(|r| HistoryEntry {
        at: r.at,
        text: report_line(r),
    }));
    history.sort_by_key(|h| h.at);
    // An answer is its newest report's, whatever the kind; the database keeps it, so no file does.
    let answer = reports.iter().rev().find_map(|r| r.answer.clone());
    let name = project.map_or_else(String::new, |p| p.name);
    Ok(TaskDetail {
        summary: summary(&row, &name),
        goal: row.goal.clone(),
        history,
        answer,
    })
}

/// Whether `row` matches a `find_tasks` key. A task (`T-0017`) matches its crewmates; when no
/// crewmate is in it, the crewmate numbered like it, the name crewmates had before tasks held
/// several (`labelled` says which).
fn matches_key(row: &TaskRow, key: &str, labelled: bool) -> bool {
    let key = key.trim();
    if labelled && let Ok(label) = key.parse::<TaskLabelId>() {
        return row.label == label;
    }
    if let Ok(id) = key.parse::<TaskId>() {
        return row.id == id;
    }
    let digits = key.trim_start_matches('#');
    if let Ok(pr) = digits.parse::<i64>() {
        return row.pr_number == Some(pr);
    }
    // The recorded branch, a descriptive one or an older task's `qm/task-0042`.
    !key.is_empty() && row.branch == key
}

/// `find_tasks`.
pub(crate) async fn find_tasks(
    svc: &TaskService,
    caller: &Caller,
    query: FindTasksArgs,
) -> Result<FoundTasks, ToolFailure> {
    let reader = *caller;
    let (rows, projects, readable) = svc
        .db()
        .call(move |conn| {
            let rows = task_store::all(conn)?;
            // A secondmate's search keeps to its own tasks and its scope's.
            let mut readable = None;
            if crate::secondmates::scope::bound_by(&reader).is_some() {
                let mut ids = std::collections::HashSet::new();
                for row in &rows {
                    if crate::secondmates::scope::may_read_task(conn, &reader, row.id)?.is_ok() {
                        ids.insert(row.id);
                    }
                }
                readable = Some(ids);
            }
            let mut projects: HashMap<ProjectId, ProjectRow> = HashMap::new();
            for p in active_projects(conn)? {
                projects.insert(p.id, p);
            }
            for row in &rows {
                if !projects.contains_key(&row.project)
                    && let Some(p) = load_project(conn, row.project)?
                {
                    projects.insert(p.id, p);
                }
            }
            Ok((rows, projects, readable))
        })
        .await
        .map_err(|e| failed(&e))?;
    let project_filter = query.project.as_deref().map(str::trim);
    if let Some(name) = project_filter
        && !projects.values().any(|p| p.name.eq_ignore_ascii_case(name))
    {
        return Err(refuse(format!("There's no project called {name}")));
    }
    let text = query.text.as_deref().map(|t| t.trim().to_lowercase());
    let labelled = query
        .key
        .as_deref()
        .and_then(|k| k.trim().parse::<TaskLabelId>().ok())
        .is_some_and(|label| rows.iter().any(|r| r.label == label));
    let mut matching: Vec<&TaskRow> = rows
        .iter()
        .filter(|r| {
            if query.status.is_empty() {
                r.status != TaskStatus::Closed
            } else {
                query.status.contains(&r.status)
            }
        })
        .filter(|r| {
            project_filter.is_none_or(|name| {
                projects
                    .get(&r.project)
                    .is_some_and(|p| p.name.eq_ignore_ascii_case(name))
            })
        })
        .filter(|r| {
            query
                .key
                .as_deref()
                .is_none_or(|k| matches_key(r, k, labelled))
        })
        .filter(|r| {
            text.as_deref().is_none_or(|t| {
                t.is_empty()
                    || r.title.to_lowercase().contains(t)
                    || r.goal.to_lowercase().contains(t)
            })
        })
        .filter(|r| query.parent.is_none_or(|p| r.parent == Some(p)))
        .filter(|r| readable.as_ref().is_none_or(|ids| ids.contains(&r.id)))
        .collect();
    // Newest first.
    matching.reverse();
    let mut per_project: HashMap<ProjectId, usize> = HashMap::new();
    let mut tasks = Vec::new();
    let mut more = 0_u32;
    for row in matching {
        let n = per_project.entry(row.project).or_default();
        if *n >= PER_PROJECT || tasks.len() >= IN_ALL {
            more += 1;
            continue;
        }
        *n += 1;
        let name = projects.get(&row.project).map_or("", |p| p.name.as_str());
        tasks.push(summary(row, name));
    }
    Ok(FoundTasks { tasks, more })
}
