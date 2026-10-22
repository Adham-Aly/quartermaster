//! A plan task's plan, as the task service drives it: `plan_check`, what `report` does for a plan,
//! the user's approval, and a pasted reply passed on to the planner.
//!
//! Under normal autonomy the planner questions the user through the plan's page. A round's report
//! moves the task to `waiting` (on the user, not the supervisor), with no supervisor wake: the app
//! posts the plan's card in the Quartermaster's chat. Only the user's approval (the card's button,
//! or `approve_plan` on a turn the user's message started) moves it to `reported` and wakes the
//! supervisor to start the build. Under auto autonomy the plan's report is an ordinary report.
//!
//! Everything about the plan stays in its folder in the scratch root ([`crate::plans`]).

use super::{TaskService, failed, load_project, new_wake, refuse};
use crate::inbox::Inbox;
use crate::plans::{self, PlanState, Stage};
use qm_plans::reply::PlanReply;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::Ack;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{PlanMode, ReportOutcome, Role, TaskKind, TaskStatus, WakeKind};
use qm_types::ids::TaskId;
use std::path::{Path, PathBuf};

/// `events.kind` of a plan's approval.
pub const APPROVED_EVENT: &str = "task.plan_approved";
/// `events.kind` of a plan round put to the user.
pub const ROUND_EVENT: &str = "task.plan_round";

/// What a plan task's report does, decided before its transaction.
#[derive(Clone, Debug)]
pub(crate) struct PlanReport {
    pub mode: PlanMode,
    /// The state to save once the report is recorded.
    pub state: PlanState,
    /// For normal autonomy: the round goes to the user (no supervisor wake, a card instead).
    pub to_user: bool,
}

impl TaskService {
    /// The plan folder of `task`.
    pub(crate) fn plan_dir(&self, task: TaskId) -> PathBuf {
        self.deps().data_dir.task(task).plan
    }

    /// The plan mode `row`'s project's autonomy gives now; `None` for a task that isn't a plan.
    pub(crate) async fn live_plan_mode(
        &self,
        row: &TaskRow,
    ) -> Result<Option<PlanMode>, ToolFailure> {
        if row.kind != TaskKind::Plan {
            return Ok(None);
        }
        let project = row.project;
        let project = self
            .db()
            .call(move |conn| load_project(conn, project))
            .await
            .map_err(|e| failed(&e))?
            .ok_or_else(|| refuse(format!("{}'s project is gone", row.id)))?;
        Ok(Some(PlanMode::for_autonomy(project.autonomy)))
    }

    /// Your autonomy change for the project named `project` reaches its open plans.
    pub async fn autonomy_changed(&self, project: &str) {
        let name = project.to_owned();
        match self
            .db()
            .call(move |conn| qm_store::projects::active_by_name(conn, &name))
            .await
        {
            Ok(Some(p)) => autonomy_changed(self, p.id).await,
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "couldn't read the project whose autonomy changed");
            }
        }
    }

    async fn project_name(&self, row: &TaskRow) -> String {
        let project = row.project;
        self.db()
            .call(move |conn| load_project(conn, project))
            .await
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default()
    }
}

/// The planner's own plan task, in the mode its session was started in, which must still be the
/// live one: a mode change restarts the session after this turn.
async fn planner_task(
    svc: &TaskService,
    caller: &Caller,
) -> Result<(TaskRow, PlanMode, PlanState, PathBuf), ToolFailure> {
    let row = super::report::callers_task(svc, caller).await?;
    let Some(live) = svc.live_plan_mode(&row).await? else {
        return Err(refuse(format!("{} isn't a plan task", row.id)));
    };
    if caller.plan != Some(live) {
        return Err(refuse(format!(
            "autonomy for this project changed, so this plan is now planned in {live} mode; end \
             your turn and your session restarts in the new mode"
        )));
    }
    let dir = svc.plan_dir(row.id);
    let state = load_state(&dir)?;
    Ok((row, live, state, dir))
}

fn load_state(dir: &Path) -> Result<PlanState, ToolFailure> {
    plans::load(dir).map_err(|e| failed(&e))?.ok_or_else(|| {
        refuse("this task's plan folder is gone; tell your supervisor with report blocked")
    })
}

/// `plan_check`.
pub(crate) async fn plan_check(svc: &TaskService, caller: &Caller) -> Result<String, ToolFailure> {
    let (row, mode, state, dir) = planner_task(svc, caller).await?;
    let project = svc.project_name(&row).await;
    let task = row.id;
    tokio::task::spawn_blocking(move || {
        let checked = plans::check(&dir, mode, &state).map_err(refuse)?;
        let counts = format!(
            "{} diagram{}, {} mockup{}, {} image{}",
            checked.diagrams,
            if checked.diagrams == 1 { "" } else { "s" },
            checked.mockups,
            if checked.mockups == 1 { "" } else { "s" },
            checked.images,
            if checked.images == 1 { "" } else { "s" },
        );
        let notes = if checked.notes.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = checked.notes.iter().map(ToString::to_string).collect();
            format!("\nNotes:\n{}", lines.join("\n"))
        };
        match mode {
            PlanMode::Internal => Ok(format!("plan.md OK · {counts}{notes}")),
            PlanMode::Interactive => {
                // The page as the next round will show it, so the planner can look before it
                // reports.
                let mut next = state.clone();
                next.round += 1;
                next.stage = if checked.questions.is_empty() {
                    Stage::Final
                } else {
                    Stage::WithUser
                };
                next.locked = None;
                let checked =
                    plans::render(&dir, task, &project, &next, state.fingerprints.clone())
                        .map_err(refuse)?;
                Ok(format!(
                    "plan.md OK · round {} draft · {} · page at file://{}{notes}",
                    next.round,
                    checked.summary(),
                    dir.join(qm_plans::PAGE_FILE).display()
                ))
            }
        }
    })
    .await
    .map_err(|e| failed(&e))?
}

/// Before a plan task's report is recorded: checks `plan.md` for the live mode and works out what
/// the report does. `None` for a report that isn't a finished round (blocked, a decision).
pub(crate) async fn before_report(
    svc: &TaskService,
    caller: &Caller,
    outcome: ReportOutcome,
) -> Result<Option<PlanReport>, ToolFailure> {
    let (row, mode, state, dir) = planner_task(svc, caller).await?;
    if outcome != ReportOutcome::Done {
        return Ok(None);
    }
    let project = svc.project_name(&row).await;
    let task = row.id;
    let at = svc.clock().now();
    tokio::task::spawn_blocking(move || {
        let checked = plans::check(&dir, mode, &state).map_err(refuse)?;
        let mut next = state.clone();
        next.round += 1;
        next.open = checked.questions.iter().map(|q| q.id.clone()).collect();
        next.pending_reply = false;
        next.locked = None;
        next.approved_at = None;
        next.stage = match mode {
            PlanMode::Internal => Stage::Approved,
            PlanMode::Interactive if !checked.questions.is_empty() => Stage::WithUser,
            PlanMode::Interactive if state.take_recommended => {
                next.approved_at = Some(local_time(at));
                Stage::Approved
            }
            PlanMode::Interactive => Stage::Final,
        };
        // A round under normal autonomy goes to the user; one the user already approved (with
        // the recommendations) is reported like any finished plan.
        let to_user = mode == PlanMode::Interactive && next.stage != Stage::Approved;
        if mode == PlanMode::Interactive {
            let checked = plans::render(&dir, task, &project, &next, state.fingerprints.clone())
                .map_err(refuse)?;
            next.fingerprints = checked.fingerprints();
        } else {
            next.fingerprints = checked.fingerprints();
        }
        Ok(Some(PlanReport {
            mode,
            state: next,
            to_user,
        }))
    })
    .await
    .map_err(|e| failed(&e))?
}

/// "14:30", in the Mac's local time.
fn local_time(at: jiff::Timestamp) -> String {
    let zoned = at.to_zoned(jiff::tz::TimeZone::system());
    format!("{:02}:{:02}", zoned.hour(), zoned.minute())
}

/// After a plan task's report is recorded: saves the plan's state and, for a round that goes to
/// the user, posts its card in the Quartermaster's chat.
pub(crate) async fn after_report(svc: &TaskService, task: TaskId, report: PlanReport) {
    let dir = svc.plan_dir(task);
    let mut state = report.state;
    if report.to_user {
        let text = match state.stage {
            Stage::WithUser => format!(
                "{task}'s plan, round {}, has {} for you",
                state.round,
                questions(state.open.len())
            ),
            Stage::Approved => format!("{task}'s final plan is approved, with the recommendations"),
            Stage::Final | Stage::Drafting => format!("{task}'s plan is ready for your approval"),
        };
        match svc
            .deps()
            .chat
            .app_note(svc.deps().seat_agent, None, &text)
            .await
        {
            Ok(message) => state.card = Some(message),
            Err(e) => tracing::error!(%task, error = %e, "couldn't post the plan's card"),
        }
    }
    let saved = state.clone();
    let result = tokio::task::spawn_blocking(move || plans::update(&dir, |s| *s = saved)).await;
    if !matches!(result, Ok(Ok(Some(_)))) {
        tracing::error!(%task, "couldn't save the plan's state");
    }
    svc.changed();
}

fn questions(n: usize) -> String {
    if n == 1 {
        "1 question".to_owned()
    } else {
        format!("{n} questions")
    }
}

/// The supervisor's wake for a plan that became ready to build, and the text of its event.
pub(crate) fn ready_text(task: TaskId, row: &TaskRow, how: &str) -> String {
    format!(
        "{task}'s plan ({}) {how}. Start the build the user asked for with builds_on {task}, then \
         settle_task {task}. The plan is in its plan folder; never relay it in chat.",
        row.title
    )
}

/// Who approves.
#[derive(Clone, Copy, Debug)]
pub enum Approver<'a> {
    /// An orchestrator's `approve_plan` on a turn the user's message started.
    Agent(&'a Caller),
    /// The Approve button on the plan's card.
    Card,
}

/// Approves `task`'s plan for the user.
pub(crate) async fn approve(
    svc: &TaskService,
    by: Approver<'_>,
    task: TaskId,
    take_recommended: bool,
) -> Result<String, ToolFailure> {
    let row = super::waits::load_task(svc, task).await?;
    if let Approver::Agent(caller) = by
        && caller.role == Role::Secondmate
    {
        super::waits::check_direct(svc, caller, &row).await?;
    }
    if row.kind != TaskKind::Plan {
        return Err(refuse(format!("{task} isn't a plan task")));
    }
    if !super::lifecycle::is_open(row.status) {
        return Err(refuse(format!("{task} is closed; its plan is gone")));
    }
    if svc.live_plan_mode(&row).await? == Some(PlanMode::Internal) {
        return Err(refuse(format!(
            "{task} is planned under auto autonomy; its plan needs no approval"
        )));
    }
    let dir = svc.plan_dir(task);
    let state = load_state(&dir)?;
    match state.stage {
        Stage::Approved => return Ok(format!("{task}'s plan is already approved.")),
        Stage::Drafting => {
            return Err(refuse(format!(
                "{task} is writing its plan; approve when the next round is back"
            )));
        }
        _ if state.pending_reply => {
            return Err(refuse(format!(
                "{task} is updating its plan with the user's reply; approve when the next round is back"
            )));
        }
        Stage::WithUser if !take_recommended => {
            return Err(refuse(format!(
                "{task}'s plan still has {} open; the user answers them in the page, or approves \
                 with the planner's recommendations (take_recommended)",
                questions(state.open.len())
            )));
        }
        Stage::WithUser | Stage::Final => {}
    }
    let at = svc.clock().now();
    let by_agent = match by {
        Approver::Agent(caller) => Some(caller.agent),
        Approver::Card => None,
    };
    if state.stage == Stage::WithUser {
        return take_recommendations(svc, &row, dir).await;
    }
    finish_approval(svc, &row, dir, by_agent, at).await
}

/// Approval with the planner's recommendations while questions are open: the planner settles
/// them and reports a final plan, which counts as approved.
async fn take_recommendations(
    svc: &TaskService,
    row: &TaskRow,
    dir: PathBuf,
) -> Result<String, ToolFailure> {
    let task = row.id;
    let dir2 = dir.clone();
    tokio::task::spawn_blocking(move || {
        plans::update(&dir2, |s| {
            s.take_recommended = true;
            s.pending_reply = true;
        })
    })
    .await
    .map_err(|e| failed(&e))?
    .map_err(|e| failed(&e))?;
    steer(
        svc,
        row,
        "The user approved your plan and asked you to take your recommended answer for every \
         open question. Settle them in plan.md, remove the question blocks, add no new \
         questions, check it and report.",
    )
    .await?;
    Ok(format!(
        "Approved with the recommendations. {task} settles its open questions; its final plan \
         counts as approved."
    ))
}

/// The final plan approved: the page shows it, the task moves to `reported`, and the supervisor
/// is woken to start the build unless it approved itself.
async fn finish_approval(
    svc: &TaskService,
    row: &TaskRow,
    dir: PathBuf,
    by_agent: Option<qm_types::ids::AgentId>,
    at: jiff::Timestamp,
) -> Result<String, ToolFailure> {
    let task = row.id;
    let shown = local_time(at);
    let project = svc.project_name(row).await;
    let shown2 = shown.clone();
    tokio::task::spawn_blocking(move || {
        let updated = plans::update(&dir, |s| {
            s.stage = Stage::Approved;
            s.approved_at = Some(shown2);
        })?;
        if let Some((state, ())) = updated
            && let Err(why) =
                plans::render(&dir, task, &project, &state, state.fingerprints.clone())
        {
            tracing::warn!(%task, %why, "couldn't re-render the approved plan's page");
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|e| failed(&e))?
    .map_err(|e| failed(&e))?;
    let text = ready_text(task, row, "was approved by the user");
    let wake_supervisor = by_agent != Some(row.supervisor);
    let from = row.status;
    let supervisor = row.supervisor;
    svc.write(move |tx, notify| {
        if from == TaskStatus::Waiting {
            task_store::set_waiting_on(tx, task, None)?;
            task_store::set_status(tx, task, from, TaskStatus::Reported, at)?;
            super::waits::release_waiters(tx, notify, task, TaskStatus::Reported, at)?;
        }
        task_store::event(
            tx,
            APPROVED_EVENT,
            task,
            by_agent,
            &format!("the user approved {task}'s plan"),
            serde_json::json!({}),
            at,
        )?;
        if wake_supervisor {
            let wake = new_wake(
                supervisor,
                WakeKind::Reported,
                task,
                format!("plan_approved:{task}"),
                &text,
                &format!("{task}'s plan was approved"),
                serde_json::json!({}),
                at,
            );
            Inbox::post(tx, &wake, notify)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| failed(&e))?;
    svc.changed();
    Ok(format!(
        "Approved at {shown}. Start the build the user asked for with builds_on {task}, then \
         settle_task {task}."
    ))
}

/// A message to the planner, as from its supervisor.
async fn steer(svc: &TaskService, row: &TaskRow, text: &str) -> Result<(), ToolFailure> {
    let caller = Caller {
        agent: row.supervisor,
        role: Role::Quartermaster,
        turn: None,
        task: None,
        secondmate: None,
        plan: None,
    };
    super::waits::message_task(svc, &caller, row.id, text.to_owned())
        .await
        .map(drop)
}

/// The plan replies in a supervisor's message to a plan task, checked against the plan and the
/// user's own message: each must be for this plan's current round and appear word for word in
/// `user_text`, the message of the user's that started the supervisor's turn. On success the plan
/// records the reply; the text then goes to the planner as usual.
pub(crate) async fn record_replies(
    svc: &TaskService,
    row: &TaskRow,
    replies: &[PlanReply],
    user_text: Option<&str>,
) -> Result<Option<String>, ToolFailure> {
    if replies.is_empty() {
        return Ok(None);
    }
    let task = row.id;
    if row.kind != TaskKind::Plan {
        return Err(refuse(format!(
            "that plan reply is for a plan task, and {task} isn't one"
        )));
    }
    if svc.live_plan_mode(row).await? == Some(PlanMode::Internal) {
        return Err(refuse(format!(
            "{task} is planned under auto autonomy now; it decides its open points itself, so \
             tell the user their reply wasn't sent"
        )));
    }
    let Some(user_text) = user_text else {
        return Err(refuse(
            "a plan reply is passed on only from the user's own message, on the turn it started",
        ));
    };
    let user_text = user_text.replace("\r\n", "\n");
    let dir = svc.plan_dir(task);
    let state = load_state(&dir)?;
    for reply in replies {
        if reply.task != task.to_string() {
            return Err(refuse(format!(
                "that plan reply is for {}, not {task}; pass it to {} instead",
                reply.task, reply.task
            )));
        }
        if reply.plan_uid != state.uid {
            return Err(refuse(format!(
                "that plan reply is for an older plan of {task}; tell the user to open the current page from its card"
            )));
        }
        if reply.round != state.round {
            return Err(refuse(format!(
                "that plan reply is for round {}, but {task} is on round {}; tell the user to answer the current page",
                reply.round, state.round
            )));
        }
        if !user_text.contains(&reply.raw) {
            return Err(refuse(
                "pass the user's plan reply exactly as they pasted it, header to closing line",
            ));
        }
    }
    if !matches!(
        state.stage,
        Stage::WithUser | Stage::Final | Stage::Approved
    ) {
        return Err(refuse(format!(
            "{task} is writing its next round now; tell the user to paste their reply again when it's back"
        )));
    }
    let round = state.round;
    tokio::task::spawn_blocking(move || {
        plans::update(&dir, |s| {
            if s.last_answered_round < round {
                s.last_answered_round = round;
                s.user_rounds += 1;
            }
            s.pending_reply = true;
        })
    })
    .await
    .map_err(|e| failed(&e))?
    .map_err(|e| failed(&e))?;
    let (answered, open, sections) = replies.iter().fold((0, 0, 0), |acc, r| {
        let (a, o, s) = r.counts();
        (acc.0 + a, acc.1 + o, acc.2 + s)
    });
    svc.changed();
    Ok(Some(format!(
        "The user's reply to round {round} went to {task}: {answered} answered, {open} left open, \
         {sections} section notes."
    )))
}

/// The plan approval's tool: `approve_plan`.
pub(crate) async fn approve_plan(
    svc: &TaskService,
    caller: &Caller,
    task: TaskId,
    take_recommended: bool,
) -> Result<Ack, ToolFailure> {
    let note = approve(svc, Approver::Agent(caller), task, take_recommended).await?;
    Ok(Ack { note })
}

/// Whether a build may start on `plan`: its plan is approved (or finished under auto).
pub(crate) fn ready_to_build(svc: &TaskService, plan: TaskId) -> Result<(), String> {
    let dir = svc.plan_dir(plan);
    match plans::load(&dir) {
        Ok(Some(state)) if state.stage == Stage::Approved => Ok(()),
        Ok(Some(state)) => Err(match state.stage {
            Stage::Final => format!(
                "{plan}'s plan isn't approved yet; the user approves it on its card or in chat"
            ),
            _ => format!(
                "{plan}'s plan isn't finished; the user answers its questions in its page first"
            ),
        }),
        Ok(None) => Err(format!("{plan}'s plan is gone")),
        Err(e) => Err(format!("{plan}'s plan can't be read: {e}")),
    }
}

/// Your autonomy change for `project` reaches its open plans: under auto a plan's page closes to
/// replies and its planner carries on deciding alone; under normal an unapproved plan's planner
/// turns what it decided alone into questions. A running planner's session restarts in the new
/// mode at its next turn (the runner checks); this wakes the ones that are waiting.
async fn autonomy_changed(svc: &TaskService, project: qm_types::ids::ProjectId) {
    let rows = match svc
        .db()
        .call(move |conn| {
            let mut rows = Vec::new();
            for (id, kind) in task_store::open_in_project(conn, project)? {
                if kind == TaskKind::Plan
                    && let Some(row) = task_store::load(conn, id)?
                {
                    rows.push(row);
                }
            }
            Ok(rows)
        })
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "couldn't read the plans an autonomy change reaches");
            return;
        }
    };
    for row in rows {
        if let Err(e) = plan_autonomy_changed(svc, &row).await {
            tracing::warn!(task = %row.id, error = %e.render(), "couldn't move a plan to its new autonomy");
        }
    }
}

async fn plan_autonomy_changed(svc: &TaskService, row: &TaskRow) -> Result<(), ToolFailure> {
    let Some(mode) = svc.live_plan_mode(row).await? else {
        return Ok(());
    };
    let dir = svc.plan_dir(row.id);
    let project = svc.project_name(row).await;
    let task = row.id;
    let steer_text =
        tokio::task::spawn_blocking(move || -> Result<Option<&'static str>, String> {
            let Some((state, text)) = plans::update(&dir, |s| match (mode, s.stage) {
            (_, Stage::Approved) => None,
            (PlanMode::Internal, _) => {
                s.locked = Some(
                    "The project switched to auto autonomy, so the planner decides the open \
                     points itself. This page takes no more replies."
                        .to_owned(),
                );
                s.stage = Stage::Drafting;
                Some(
                    "The project's autonomy changed to auto. Finish the plan without the user: \
                     decide every open point yourself, then plan_check and report done.",
                )
            }
            (PlanMode::Interactive, _) => {
                s.locked = None;
                Some(
                    "The project's autonomy changed to normal. Turn what you decided alone, and \
                     anything not crystal clear, into questions for the user, then plan_check and \
                     report done.",
                )
            }
        })
        .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
            // The page, if there is one, shows it's closed (or open again).
            if state.round > 0 && dir.join(qm_plans::PAGE_FILE).is_file() {
                let _ = plans::render(
                    &dir,
                    task,
                    &project,
                    &state,
                    std::collections::BTreeMap::new(),
                );
            }
            Ok(text)
        })
        .await
        .map_err(|e| failed(&e))?
        .map_err(ToolFailure::Failed)?;
    svc.changed();
    match steer_text {
        Some(text) if row.status != TaskStatus::Reported => steer(svc, row, text).await,
        _ => Ok(()),
    }
}
