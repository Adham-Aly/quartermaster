//! The crew tree and the task pane, display-ready.
//!
//! [`load`] reads `qm_store::crew_view::crew_data` and builds the rows under the Quartermaster's
//! root row (which `api.rs` writes): its open tasks at depth 1, then its secondmates at depth 1,
//! each followed by its own open tasks at depth 2, tasks oldest first. Each task gets a
//! [`TaskDetail`] for the task pane, with token use summed over its crewmate's turns.
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
    AgentRef, Attention, Command, CrewRow, Snapshot, Styled, TaskDetail, TaskRow, Tone, Waiting,
};
use qm_types::clock::Clock;
use qm_types::domain::{HeldKind, TaskStatus, VerificationLevel};
use qm_types::ids::AgentId;
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

/// The view of `data` at `now` (pure, for snapshot fixtures).
pub fn build(data: &CrewData, now: Timestamp, started: Option<Timestamp>) -> CrewView {
    let secondmate_agents: Vec<AgentId> = data.secondmates.iter().filter_map(|s| s.agent).collect();
    let mut view = CrewView::default();
    // The Quartermaster's own tasks, and any whose supervisor isn't shown (so no work is hidden).
    for task in data.tasks.iter().filter(|t| {
        Some(t.supervisor) == data.quartermaster || !secondmate_agents.contains(&t.supervisor)
    }) {
        push_task(&mut view, task, 1, now, started);
    }
    for secondmate in &data.secondmates {
        let status = if secondmate.working {
            SECONDMATE_WORKING
        } else {
            SECONDMATE_IDLE
        };
        let mut row = CrewRow::new(
            AgentRef::Secondmate { id: secondmate.id },
            1,
            format!("{} {}", secondmate.id, secondmate.name),
            status,
        );
        row.busy = secondmate.working;
        view.crew.push(row);
        let Some(agent) = secondmate.agent else {
            continue;
        };
        for task in data.tasks.iter().filter(|t| t.supervisor == agent) {
            push_task(&mut view, task, 2, now, started);
        }
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

fn push_task(
    view: &mut CrewView,
    task: &OpenTask,
    depth: u8,
    now: Timestamp,
    started: Option<Timestamp>,
) {
    let (status, waiting) = status_word(task, now);
    let mut row = CrewRow::new(
        AgentRef::Crewmate { task: task.task },
        depth,
        format!("{} {}", task.task, task.kind),
        status.clone(),
    );
    row.waiting = waiting;
    row.needs_you = waiting.is_some();
    row.busy = task.in_turn;
    view.crew.push(row);
    view.tasks
        .push(detail(task, &status, waiting, now, started));
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
        // Never loaded (the store leaves them out); worded plainly should one slip through.
        TaskStatus::Closed | TaskStatus::Failed | TaskStatus::Stopped | TaskStatus::Discarded => {
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
    status: &str,
    waiting: Option<Waiting>,
    now: Timestamp,
    started: Option<Timestamp>,
) -> TaskDetail {
    let text = |s: String| vec![Styled::new(s, Tone::Text)];
    let mut d = TaskDetail::new(task.task, task.title.clone());
    d.rows
        .push(TaskRow::new(Some("project"), text(task.project.clone())));

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn crew_view_shows_null_branch_folder_tasks_alongside_git_tasks() {
        let mut conn = Connection::open_in_memory().expect("open fixture");
        qm_store::schema::migrate(&mut conn).expect("migrate");
        conn.execute_batch(
            "INSERT INTO agents (id, role, home, status, created_at)
             VALUES (1, 'quartermaster', '/fixture/seat', 'idle', 0);
             INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                                   merge_method, created_at, seq)
             VALUES (1, 'personal', '/fixture/personal', 'workspace', '', 'folder', 'squash', 0, 1),
                    (2, 'app', '/fixture/app', 'linked', 'main', 'branch_only', 'squash', 0, 2);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                                delivery_mode, branch, status, created_at, seq)
             VALUES (1, 1, 1, 'fix', 'Folder task', 'Show folder task',
                     'folder', NULL, 'held', 0, 1),
                    (2, 2, 1, 'fix', 'Git task', 'Keep git branch',
                     'branch_only', 'qm/task-0002', 'held', 1, 2);",
        )
        .expect("seed open tasks");

        let data = crew_view::crew_data(&conn).expect("load crew including NULL branch");
        assert_eq!(data.tasks.len(), 2);
        assert_eq!(data.tasks[0].branch, None);
        assert_eq!(data.tasks[1].branch.as_deref(), Some("qm/task-0002"));

        let view = build(&data, Timestamp::UNIX_EPOCH, None);
        assert_eq!(view.crew.len(), 2);
        assert_eq!(view.tasks.len(), 2);
        for (row, task) in view.crew.iter().zip(&data.tasks) {
            assert_eq!(row.agent, AgentRef::Crewmate { task: task.task });
            assert_eq!(row.status, "decide");
        }
        let folder = &view.tasks[0];
        assert_eq!(folder.task, data.tasks[0].task);
        assert_eq!(folder.title, "Folder task");
        assert!(
            !folder
                .rows
                .iter()
                .any(|r| r.label.as_deref() == Some("branch"))
        );
        assert!(
            folder.rows.iter().any(|r| {
                r.label.as_deref() == Some("project") && r.value[0].text == "personal"
            })
        );
        let git = &view.tasks[1];
        assert_eq!(git.task, data.tasks[1].task);
        let branch = git
            .rows
            .iter()
            .find(|r| r.label.as_deref() == Some("branch"))
            .expect("git branch row");
        assert_eq!(branch.value[0].text, "qm/task-0002 ← main");
    }
}
