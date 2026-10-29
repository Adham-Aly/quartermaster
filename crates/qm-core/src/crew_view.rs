//! The crew tree and the task pane, display-ready.
//!
//! [`load`] reads `qm_store::crew_view::crew_data` and builds the rows under the Quartermaster's
//! root row (which `api.rs` writes): its visible crewmates at depth 1, then its secondmates at depth
//! 1, each followed by its own visible crewmates at depth 2. Open tasks and closed reports
//! awaiting explicit dismissal stay visible. Under each supervisor the crewmates are
//! kept together by task (`T-`), tasks in the order their oldest crewmate there was made and
//! crewmates oldest first, and each row carries its task's heading ([`CrewTask`]), which the tree
//! draws above them: supervisor → task → crewmates. A task with no visible crewmate has no rows, so
//! it drops out. Each crewmate gets a [`TaskDetail`] for the task pane, with token use summed over
//! its turns.
//!
//! [`CrewPart`] puts both into every snapshot, as an [`ApiPart`] registered in the binary's
//! wiring.

use crate::api::{ApiPart, Handled};
use jiff::Timestamp;
use qm_store::StoreError;
use qm_store::crew_view::{self, CrewData, OpenTask, TokenUse};
use qm_store::db::Db;
use qm_types::BoxFuture;
use qm_types::api::{
    AgentRef, Attention, Command, CrewPhase, CrewRow, CrewTask, Snapshot, Styled, TaskDetail,
    TaskRow, Tone, Waiting,
};
use qm_types::clock::Clock;
use qm_types::domain::{HeldKind, TaskStatus, VerificationLevel};
use qm_types::ids::{AgentId, TaskId, TaskLabelId};
use std::fmt;
use std::sync::Arc;

/// A secondmate's status words.
pub const SECONDMATE_IDLE: &str = "idle";
pub const SECONDMATE_WORKING: &str = "working";

/// The crew tree's rows below the root, and a task pane for each task in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrewView {
    pub crew: Vec<CrewRow>,
    pub tasks: Vec<TaskDetail>,
    /// The status bar's counts.
    pub attention: Attention,
}

/// Reads the crew and builds the view at the clock's current time.
/// `started` is when this server started: token totals of tasks created before it are partial,
/// because the turns they sum are kept only in memory.
pub async fn load(
    db: &Db,
    clock: &dyn Clock,
    started: Option<Timestamp>,
) -> Result<CrewView, StoreError> {
    let data = db.call(|conn| crew_view::crew_data(conn)).await?;
    Ok(build(&data, clock.now(), started))
}

/// The view of `data` at `now`.
pub fn build(data: &CrewData, now: Timestamp, started: Option<Timestamp>) -> CrewView {
    let secondmate_agents: Vec<AgentId> = data.secondmates.iter().filter_map(|s| s.agent).collect();
    let mut view = CrewView::default();
    // The Quartermaster's own tasks, and any whose supervisor isn't shown (so no work is hidden).
    let own: Vec<&OpenTask> = data
        .tasks
        .iter()
        .filter(|t| {
            Some(t.supervisor) == data.quartermaster || !secondmate_agents.contains(&t.supervisor)
        })
        .collect();
    push_group(&mut view, data, &own, 1, now, started);
    for secondmate in &data.secondmates {
        let status = if secondmate.working {
            SECONDMATE_WORKING
        } else {
            SECONDMATE_IDLE
        };
        let mut row = CrewRow::new(
            AgentRef::Secondmate { id: secondmate.id },
            1,
            secondmate.name.clone(),
            status,
        );
        row.busy = secondmate.working;
        view.crew.push(row);
        let Some(agent) = secondmate.agent else {
            continue;
        };
        let theirs: Vec<&OpenTask> = data
            .tasks
            .iter()
            .filter(|t| t.supervisor == agent)
            .collect();
        push_group(&mut view, data, &theirs, 2, now, started);
    }
    view.attention = attention(data, &view.crew);
    view
}

/// The status bar's counts: tasks whose crewmate is in a turn; rows flagged for a decision or an
/// approval; and every other pending held action, so a card with no flagged row (a GitHub write, a
/// project to add, a secondmate) is still counted. A held action whose task row already carries a
/// flag is counted once, through the flag.
fn attention(data: &CrewData, crew: &[CrewRow]) -> Attention {
    let mut out = Attention::default();
    let mut flagged = Vec::new();
    for row in crew {
        let AgentRef::Crewmate { task } = row.agent else {
            continue;
        };
        match row.waiting {
            Some(Waiting::Approval) => out.approvals += 1,
            Some(Waiting::Decision) => out.decisions += 1,
            _ => continue,
        }
        flagged.push(task);
    }
    for hold in &data.pending_holds {
        if hold.task.is_some_and(|t| flagged.contains(&t)) {
            continue;
        }
        if hold.kind == HeldKind::Decision.as_str() {
            out.decisions += 1;
        } else {
            out.approvals += 1;
        }
    }
    out.running = data
        .tasks
        .iter()
        .filter(|t| t.in_turn)
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    out
}

/// One supervisor's crewmates (`tasks`, oldest first) at `depth`, kept together by task: tasks in
/// the order their first crewmate comes, crewmates in their own order within each.
fn push_group(
    view: &mut CrewView,
    data: &CrewData,
    tasks: &[&OpenTask],
    depth: u8,
    now: Timestamp,
    started: Option<Timestamp>,
) {
    let mut labels: Vec<TaskLabelId> = Vec::new();
    for task in tasks {
        if !labels.contains(&task.label) {
            labels.push(task.label);
        }
    }
    for label in labels {
        let members: Vec<&OpenTask> = tasks.iter().copied().filter(|t| t.label == label).collect();
        let heading = heading(label, &members);
        for task in members {
            push_task(view, data, task, heading.clone(), depth, now, started);
        }
    }
}

/// A task's friendly heading over its crewmates under one supervisor, and project summary.
fn heading(label: TaskLabelId, members: &[&OpenTask]) -> CrewTask {
    let mut projects: Vec<&str> = Vec::new();
    for task in members {
        if !projects.contains(&task.project.as_str()) {
            projects.push(&task.project);
        }
    }
    let mut project = projects
        .first()
        .map(|p| (*p).to_owned())
        .unwrap_or_default();
    if projects.len() > 1 {
        project = format!("{project} +{}", projects.len() - 1);
    }
    let name = members
        .iter()
        .find_map(|t| t.label_name.clone())
        .or_else(|| {
            members
                .first()
                .map(|t| qm_policy::names::task_name_from_title(&t.title))
        })
        .unwrap_or_default();
    CrewTask::new(label, name.clone()).named(name, project)
}

/// Where a task's work is, for the crew list's groups.
fn phase(task: &OpenTask) -> CrewPhase {
    match task.status {
        TaskStatus::Queued | TaskStatus::Waiting | TaskStatus::Held | TaskStatus::Stuck => {
            CrewPhase::Waiting
        }
        TaskStatus::Settling if task.pending_merge => CrewPhase::Waiting,
        TaskStatus::Reported | TaskStatus::Landed | TaskStatus::Closed => CrewPhase::Done,
        _ => CrewPhase::Working,
    }
}

fn push_task(
    view: &mut CrewView,
    data: &CrewData,
    task: &OpenTask,
    heading: CrewTask,
    depth: u8,
    now: Timestamp,
    started: Option<Timestamp>,
) {
    let (status, waiting) = status_word(task, now);
    let mut row = CrewRow::new(
        AgentRef::Crewmate { task: task.task },
        depth,
        task.kind.to_string(),
        status.clone(),
    );
    let task_name = heading.name.clone();
    row.task = Some(heading);
    row.kind = task.kind.to_string();
    row.phase = phase(task);
    row.waiting = waiting;
    row.needs_you = waiting.is_some();
    row.busy = task.in_turn;
    view.crew.push(row);
    let with: Vec<TaskId> = data
        .tasks
        .iter()
        .filter(|t| t.label == task.label && t.task != task.task)
        .map(|t| t.task)
        .collect();
    view.tasks.push(detail(
        task, &task_name, &with, &status, waiting, now, started,
    ));
}

/// The status word the tree shows for a task, and the flag it carries.
pub fn status_word(task: &OpenTask, now: Timestamp) -> (String, Option<Waiting>) {
    let word = |w: &str| w.to_owned();
    match task.status {
        TaskStatus::Running => (running_for(task, now), None),
        TaskStatus::Queued => (word("queued"), None),
        TaskStatus::Preparing => (word("starting"), None),
        TaskStatus::Waiting => (word("waits"), None),
        TaskStatus::Held => (word("decide"), Some(Waiting::Decision)),
        TaskStatus::Stuck => (word("stuck"), Some(Waiting::Decision)),
        TaskStatus::Reported => (word("reported"), None),
        TaskStatus::InReview => (word("review"), None),
        TaskStatus::Settling if task.pending_merge => (word("approve"), Some(Waiting::Approval)),
        TaskStatus::Settling => (word("settling"), None),
        TaskStatus::Landed => (word("landed"), None),
        TaskStatus::Closed => (word("closed"), None),
        // Side exits are never loaded; worded plainly should one slip through.
        TaskStatus::Failed | TaskStatus::Stopped | TaskStatus::Discarded => {
            (task.status.to_string(), None)
        }
    }
}

/// Time since the task started: `12m`, or `1h05m` past an hour.
fn running_for(task: &OpenTask, now: Timestamp) -> String {
    let since = task.started_at.unwrap_or(task.created_at);
    let minutes = (now.as_second() - since.as_second()).max(0) / 60;
    minutes_label(minutes)
}

/// `12m`, or `1h05m` from an hour on.
pub fn minutes_label(minutes: i64) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    }
}

/// The task pane, rows in display order.
fn detail(
    task: &OpenTask,
    task_name: &str,
    with: &[TaskId],
    status: &str,
    waiting: Option<Waiting>,
    now: Timestamp,
    started: Option<Timestamp>,
) -> TaskDetail {
    let text = |s: String| vec![Styled::new(s, Tone::Text)];
    let mut d = TaskDetail::new(task.task, task.title.clone());
    d.kind = task.kind.to_string();
    d.rows
        .push(TaskRow::new(Some("project"), text(task.project.clone())));
    // The task this crewmate works towards (its heading in the crew tree), and who else is in it.
    let in_task = match with.len() {
        0 => task_name.to_owned(),
        1 => format!("{task_name} · with 1 collaborator"),
        n => format!("{task_name} · with {n} collaborators"),
    };
    d.rows.push(TaskRow::new(Some("task"), text(in_task)));

    let agent = task.agent.as_ref();
    let routing = task.routing.as_ref();
    if let Some(model) = agent
        .and_then(|a| a.model.clone())
        .or_else(|| routing.and_then(|r| r.model.clone()))
    {
        d.rows.push(TaskRow::new(Some("model"), text(model)));
    }
    if let Some(account) = agent
        .and_then(|a| a.account.clone())
        .or_else(|| routing.and_then(|r| r.account.clone()))
    {
        d.rows.push(TaskRow::new(Some("account"), text(account)));
    }
    if let Some(r) = routing {
        let effort = agent.and_then(|a| a.effort).or(r.effort);
        let tier = match effort {
            Some(effort) => format!("{} · effort {}", r.tier, effort.label()),
            None => r.tier.to_string(),
        };
        d.rows.push(TaskRow::new(Some("tier"), text(tier)));
        if !r.why.trim().is_empty() {
            d.rows.push(TaskRow::new(
                None,
                vec![Styled::new(format!("why: “{}”", r.why.trim()), Tone::Dim)],
            ));
        }
    }
    if let Some(branch) = &task.branch {
        d.rows.push(TaskRow::new(
            Some("branch"),
            text(format!("{branch} ← {}", task.target)),
        ));
    } else if let Some(folder) = &task.folder {
        d.rows
            .push(TaskRow::new(Some("folder"), text(folder.clone())));
    }
    d.rows.push(TaskRow::new(
        Some("verify"),
        text(verify_words(task.verification)),
    ));
    let status_value = if task.status == TaskStatus::Running {
        vec![
            Styled::new("running", Tone::Ok),
            Styled::new(format!(" {}", running_for(task, now)), Tone::Text),
        ]
    } else {
        let tone = match waiting {
            Some(Waiting::Approval) => Tone::Accent,
            Some(_) => Tone::Warn,
            None => Tone::Text,
        };
        vec![Styled::new(status, tone)]
    };
    d.rows.push(TaskRow::new(Some("status"), status_value));
    // Counted live as each model call finishes; nothing is shown until there is something to
    // count, never a misleading 0. The turns table lives only in memory, so a task older than
    // this server shows a partial total, and says so rather than passing it off as its lifetime.
    if task.tokens.input > 0 || task.tokens.output > 0 {
        let mut value = text(tokens_words(&task.tokens));
        if started.is_some_and(|started| task.created_at < started) {
            value.push(Styled::new(" · partial: since the app started", Tone::Dim));
        } else if task.tokens.forgotten {
            value.push(Styled::new(
                " · partial: since the last compaction",
                Tone::Dim,
            ));
        }
        d.rows.push(TaskRow::new(Some("tokens"), value));
    }

    d.activity = task
        .activity
        .iter()
        .map(|a| a.trim_start_matches('▸').trim().to_owned())
        .filter(|a| !a.is_empty())
        .collect();
    d.running = matches!(
        task.status,
        TaskStatus::Running | TaskStatus::Waiting | TaskStatus::Preparing
    );
    d
}

/// `Default (standard)`: the level's label and its wire name; `none` without a level.
pub fn verify_words(level: Option<VerificationLevel>) -> String {
    match level {
        Some(level) => format!("{} ({level})", level.label()),
        None => "none".to_owned(),
    }
}

/// `in 12k · cache read 1.2M · cache write 80k · out 91k · $3.10 · 42 turns`: each category
/// only when non-zero, the cost only where the engine reported one.
pub fn tokens_words(tokens: &TokenUse) -> String {
    let mut parts = Vec::new();
    for (label, n) in [
        ("in", tokens.fresh_input),
        ("cache read", tokens.cache_read),
        ("cache write", tokens.cache_write),
        ("out", tokens.output),
    ] {
        if n > 0 {
            parts.push(format!("{label} {}", count_words(n)));
        }
    }
    if let Some(cost) = tokens.cost_usd {
        parts.push(format!("${cost:.2}"));
    }
    match tokens.turns {
        0 => {}
        1 => parts.push("1 turn".to_owned()),
        n => parts.push(format!("{n} turns")),
    }
    parts.join(" · ")
}

/// A token count, short: `950`, `9.1k`, `182k`, `1.2M`, `34M`.
pub fn count_words(n: u64) -> String {
    qm_types::api::tokens_short(n)
}

/// Puts the crew rows and the task panes into every snapshot, as its own API part so `api.rs`
/// stays as it is.
pub struct CrewPart {
    db: Db,
    clock: Arc<dyn Clock>,
    /// When this server started (see [`load`]).
    started: Option<Timestamp>,
}

impl CrewPart {
    pub fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        let started = Some(clock.now());
        Self { db, clock, started }
    }
}

impl fmt::Debug for CrewPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrewPart").finish_non_exhaustive()
    }
}

impl ApiPart for CrewPart {
    fn handle<'a>(&'a self, _command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        None
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match load(&self.db, self.clock.as_ref(), self.started).await {
                Ok(view) => {
                    snapshot.crew.extend(view.crew);
                    snapshot.tasks = view.tasks;
                    snapshot.attention = view.attention;
                }
                Err(e) => tracing::error!(error = %e, "couldn't read the crew for the UI"),
            }
        })
    }
}
