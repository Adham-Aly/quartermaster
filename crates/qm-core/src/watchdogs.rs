//! Both watchdog ladders, wired to `qm-policy`.
//!
//! The ladders are `qm_policy::watchdog::{IdleLadder, QuietLadder}`, unchanged, with their fixed
//! timings. Each crewmate's runner owns one `Watch`, polls it whenever its loop wakes and sleeps
//! until `Watch::next_deadline`. Signs of life are read from the database each time
//! (`signs_of_life`) plus the runner's own account cut-off.
//!
//! **No step ever interrupts or stops anything.** Each step is one transaction with its event
//! (`perform`); its wake carries a dedupe key naming the step and the turn that began the spell,
//! so after a restart (when the ladder starts again from launch for the same spell) a step that
//! already fired is skipped by its key, and the stuck notice by the pending-decision check.

use crate::held::HeldService;
use crate::inbox::Inbox;
use crate::tasks::{TaskService, load_project, new_wake};
use jiff::Timestamp;
use qm_policy::holds::{card_title, stuck_choices};
use qm_policy::watchdog::{IdleLadder, IdleStep, QuietLadder, QuietStep, SignsOfLife};
use qm_store::db::Db;
use qm_store::held::{DecisionPayload, HeldChoice, HeldPayload, HoldReason, NewHeld};
use qm_store::routing_decisions::decisions_for_task;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::{StoreError, agents};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::{TaskStatus, Tier, WakeKind};
use qm_types::ids::{AgentId, TaskId, TurnId};
use rusqlite::Connection;
use std::fmt;
use std::time::{Duration, Instant};

/// `events.kind` of the quiet ladder's activity line.
pub const QUIET_EVENT: &str = "task.quiet";
/// `events.kind` of the stuck notice.
pub const STUCK_EVENT: &str = "task.stuck";
/// `events.kind` of every other step, for the record.
pub const WATCHDOG_EVENT: &str = "task.watchdog";

/// Both ladders of one crewmate, with the turn each belongs to.
#[derive(Clone, Debug, Default)]
pub(crate) struct Watch {
    idle: Option<(IdleLadder, TurnId)>,
    quiet: Option<(QuietLadder, TurnId)>,
}

/// A step that is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchStep {
    Idle(IdleStep, TurnId),
    Quiet(QuietStep, TurnId),
}

/// How a turn ended, as the idle ladder needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnEndKind {
    /// A clear `report`: no ladder.
    ClearReport,
    /// The task is in a declared wait: no ladder (and its sign of life would pause one anyway).
    Waiting,
    /// You interrupted it: you're in control until you speak again.
    InterruptedByYou,
    /// It failed at the provider: the ladder skips its nudge.
    FailedAtProvider,
    Other,
}

impl Watch {
    /// A turn started: the idle spell is over, and the quiet ladder starts.
    pub fn turn_started(&mut self, clock: &dyn Clock, turn: TurnId) {
        self.idle = None;
        self.quiet = Some((QuietLadder::start(clock), turn));
    }

    /// The engine sent something during the turn.
    pub fn engine_event(&mut self, clock: &dyn Clock) {
        if let Some((ladder, _)) = &mut self.quiet {
            ladder.on_engine_event(clock);
        }
    }

    /// A turn ended: the quiet ladder stops, and the idle ladder starts unless the end was clear.
    pub fn turn_ended(&mut self, clock: &dyn Clock, turn: TurnId, how: TurnEndKind) {
        self.quiet = None;
        self.idle = match how {
            TurnEndKind::ClearReport | TurnEndKind::Waiting | TurnEndKind::InterruptedByYou => None,
            TurnEndKind::FailedAtProvider => Some((IdleLadder::start(clock, true), turn)),
            TurnEndKind::Other => Some((IdleLadder::start(clock, false), turn)),
        };
    }

    /// After a restart: the idle spell that began with `turn` starts again from launch.
    pub fn resume_idle(&mut self, clock: &dyn Clock, turn: TurnId, failed_at_provider: bool) {
        self.idle = Some((IdleLadder::start(clock, failed_at_provider), turn));
    }

    /// Drops both ladders (the task is closing, or the supervisor took over).
    pub fn clear(&mut self) {
        self.idle = None;
        self.quiet = None;
    }

    /// The supervisor acted on the task.
    pub fn supervisor_acted(&mut self) {
        if let Some((ladder, _)) = &mut self.idle {
            ladder.supervisor_acted();
        }
    }

    /// Whether any ladder is running.
    pub fn active(&self) -> bool {
        self.idle.as_ref().is_some_and(|(l, _)| !l.finished()) || self.quiet.is_some()
    }

    /// The steps due now.
    pub fn poll(&mut self, clock: &dyn Clock, signs: &SignsOfLife) -> Vec<WatchStep> {
        let mut due = Vec::new();
        if let Some((ladder, turn)) = &mut self.idle {
            let turn = *turn;
            due.extend(
                ladder
                    .poll(clock, signs)
                    .into_iter()
                    .map(|s| WatchStep::Idle(s, turn)),
            );
        }
        if let Some((ladder, turn)) = &mut self.quiet {
            let turn = *turn;
            due.extend(
                ladder
                    .poll(clock, signs)
                    .into_iter()
                    .map(|s| WatchStep::Quiet(s, turn)),
            );
        }
        due
    }

    /// When to poll next.
    pub fn next_deadline(&self, clock: &dyn Clock, signs: &SignsOfLife) -> Option<Instant> {
        let idle = self
            .idle
            .as_ref()
            .and_then(|(l, _)| l.next_deadline(clock, signs));
        let quiet = self
            .quiet
            .as_ref()
            .and_then(|(l, _)| l.next_deadline(clock, signs));
        match (idle, quiet) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) => a,
            (None, b) => b,
        }
    }
}

/// While nothing changes the signs of life, the runner still re-reads them this often (a held
/// action answered, a wake claimed elsewhere), so a pause never outlasts its cause by more.
pub(crate) const SIGNS_RECHECK: Duration = Duration::from_secs(60);

/// The signs of life for a crewmate now.
pub(crate) async fn signs_of_life(
    db: &Db,
    clock: &dyn Clock,
    task: TaskId,
    agent: AgentId,
    cut_off_until: Option<Instant>,
) -> SignsOfLife {
    let signs = match db
        .call(move |conn| task_store::signs(conn, task, agent))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%task, error = %e, "couldn't read the signs of life; the ladders pause");
            // Unknown is never read as "no sign of life": a ladder must not fire on a read error.
            return SignsOfLife {
                declared_wait: true,
                ..SignsOfLife::default()
            };
        }
    };
    let now = clock.instant();
    let newest_running_job = signs.newest_running_job.map(|started| {
        let ago: Duration = started
            .duration_until(clock.now())
            .try_into()
            .unwrap_or(Duration::ZERO);
        now.checked_sub(ago).unwrap_or(now)
    });
    SignsOfLife {
        newest_running_job,
        message_pending: signs.pending_wakes > 0,
        question_waiting: signs.question_waiting,
        declared_wait: signs.waiting,
        account_cut_off: cut_off_until.is_some_and(|until| until > now),
    }
}

/// Who computes the stuck notice's choices when a repair or review limit stands (verification
/// fills it); the default gives the plain stuck choices.
pub trait DecisionChoices: Send + Sync + fmt::Debug {
    /// The choices for the stuck notice of `task`.
    fn stuck_choices<'a>(&'a self, task: &'a TaskRow) -> BoxFuture<'a, Vec<HeldChoice>>;
}

/// The plain stuck choices: Continue, Drop task, Discard.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainStuckChoices;

impl DecisionChoices for PlainStuckChoices {
    fn stuck_choices<'a>(&'a self, task: &'a TaskRow) -> BoxFuture<'a, Vec<HeldChoice>> {
        let _ = task;
        Box::pin(async { stuck_choices(false, Tier::Low, &[], 0) })
    }
}

/// The idle wake to the supervisor, worded for the first or the second time.
fn idle_wake_text(task: TaskId, minutes: u32, again: bool) -> String {
    let still = if again { "still " } else { "" };
    format!(
        "{task} ended a turn {minutes} minutes ago without a report and has {still}done nothing \
         since. Its last words are kept as its report (task_status {task}). Steer it with \
         message_task, or stop it with stop_task (its worktree and branch are removed either way)."
    )
}

/// The tier the task runs at and its model, for the decision card's title.
fn tier_and_model(
    conn: &Connection,
    task: TaskId,
    agent: AgentId,
) -> Result<(Tier, String), StoreError> {
    let tier = decisions_for_task(conn, task)?
        .into_iter()
        .rev()
        .find_map(|(_, d)| d.choice.map(|c| c.tier))
        .unwrap_or(Tier::Medium);
    let model = agents::load(conn, agent)?
        .and_then(|a| a.model)
        .map_or_else(|| "no model".to_owned(), |m| m.to_string());
    Ok((tier, model))
}

/// Carries out one step, in one transaction with its event. Never interrupts.
#[allow(clippy::too_many_lines)]
pub(crate) async fn perform(svc: &TaskService, task: TaskId, agent: AgentId, step: WatchStep) {
    let at = svc.clock().now();
    let choices = if matches!(step, WatchStep::Idle(IdleStep::StuckNotice, _)) {
        let row = svc
            .db()
            .call(move |conn| task_store::load(conn, task))
            .await
            .ok()
            .flatten();
        match row {
            Some(row) => Some(svc.deps().choices.stuck_choices(&row).await),
            None => return,
        }
    } else {
        None
    };
    let result = svc
        .write(move |tx, notify| {
            let Some(row) = task_store::load(tx, task)? else {
                return Ok(());
            };
            let supervisor = row.supervisor;
            let post = |notify: &mut crate::inbox::Notify,
                        target: AgentId,
                        key: String,
                        text: &str,
                        short: &str|
             -> Result<bool, StoreError> {
                let wake = new_wake(
                    target,
                    WakeKind::Watchdog,
                    task,
                    key,
                    text,
                    short,
                    serde_json::json!({}),
                    at,
                );
                Ok(Inbox::post(tx, &wake, notify)?.is_some())
            };
            match step {
                WatchStep::Idle(IdleStep::Nudge, turn) => {
                    let text = "Your last turn ended 10 minutes ago without a report. If you're \
                                done, call report; if you're blocked, report blocked or use \
                                ask_decision; if you're waiting, declare_wait.";
                    if post(
                        notify,
                        agent,
                        format!("watchdog:{task}:idle:{turn}:nudge"),
                        text,
                        "nudge",
                    )? {
                        record(tx, task, agent, "nudged after 10 quiet minutes", at)?;
                    }
                }
                WatchStep::Idle(IdleStep::WakeSupervisor, turn) => {
                    let text = idle_wake_text(task, 20, false);
                    let short = format!("{task} quiet for 20 min");
                    if post(
                        notify,
                        supervisor,
                        format!("watchdog:{task}:idle:{turn}:wake"),
                        &text,
                        &short,
                    )? {
                        record(
                            tx,
                            task,
                            agent,
                            &format!("{task} quiet for 20 minutes after its turn"),
                            at,
                        )?;
                    }
                }
                WatchStep::Idle(IdleStep::WakeSupervisorAgain, turn) => {
                    let text = idle_wake_text(task, 50, true);
                    let short = format!("{task} quiet for 50 min");
                    if post(
                        notify,
                        supervisor,
                        format!("watchdog:{task}:idle:{turn}:wake2"),
                        &text,
                        &short,
                    )? {
                        record(
                            tx,
                            task,
                            agent,
                            &format!("{task} still quiet after 50 minutes"),
                            at,
                        )?;
                    }
                }
                WatchStep::Idle(IdleStep::StuckNotice, turn) => {
                    stuck(
                        tx,
                        notify,
                        &row,
                        agent,
                        turn,
                        choices.clone().unwrap_or_default(),
                        at,
                    )?;
                }
                WatchStep::Quiet(QuietStep::ActivityLine, _) => {
                    task_store::event(
                        tx,
                        QUIET_EVENT,
                        task,
                        Some(agent),
                        &format!("{task} has sent nothing for 10 minutes mid-turn"),
                        serde_json::json!({}),
                        at,
                    )?;
                }
                WatchStep::Quiet(QuietStep::WakeSupervisor, turn) => {
                    let text = format!(
                        "{task} has been silent for 20 minutes in the middle of a turn. Nothing \
                         interrupts it automatically; stop it with stop_task if it has gone wrong."
                    );
                    let short = format!("{task} silent mid-turn");
                    if post(
                        notify,
                        supervisor,
                        format!("watchdog:{task}:quiet:{turn}:wake"),
                        &text,
                        &short,
                    )? {
                        record(
                            tx,
                            task,
                            agent,
                            &format!("{task} silent for 20 minutes mid-turn"),
                            at,
                        )?;
                    }
                }
                WatchStep::Quiet(QuietStep::LongTurn, turn) => {
                    let text = format!("{task}'s current turn has run for two hours.");
                    let short = format!("{task} turn at 2 h");
                    if post(
                        notify,
                        supervisor,
                        format!("watchdog:{task}:quiet:{turn}:long"),
                        &text,
                        &short,
                    )? {
                        record(
                            tx,
                            task,
                            agent,
                            &format!("{task}'s turn has run for two hours"),
                            at,
                        )?;
                    }
                }
            }
            Ok(())
        })
        .await;
    if let Err(e) = result {
        tracing::warn!(%task, error = %e, ?step, "a watchdog step couldn't be recorded");
    }
}

fn record(
    conn: &Connection,
    task: TaskId,
    agent: AgentId,
    summary: &str,
    at: Timestamp,
) -> Result<(), StoreError> {
    task_store::event(
        conn,
        WATCHDOG_EVENT,
        task,
        Some(agent),
        summary,
        serde_json::json!({}),
        at,
    )?;
    Ok(())
}

/// The idle ladder's last step: `running → stuck`, the decision card (once), its event and the
/// supervisor's wake.
fn stuck(
    tx: &Connection,
    notify: &mut crate::inbox::Notify,
    row: &TaskRow,
    agent: AgentId,
    turn: TurnId,
    choices: Vec<HeldChoice>,
    at: Timestamp,
) -> Result<(), StoreError> {
    let task = row.id;
    if row.status != TaskStatus::Running {
        // Waiting, held, reported or closing: the notice no longer applies.
        return Ok(());
    }
    task_store::set_status(tx, task, TaskStatus::Running, TaskStatus::Stuck, at)?;
    if !task_store::pending_decision(tx, task)? {
        let (tier, model) = tier_and_model(tx, task, agent)?;
        let project = load_project(tx, row.project)?.map_or_else(String::new, |p| p.name);
        let held = HeldService::insert(
            tx,
            &NewHeld {
                payload: HeldPayload::Decision(DecisionPayload {
                    task,
                    reason: HoldReason::Stuck,
                    detail: "No report and no sign of life for 80 minutes".to_owned(),
                    summary: None,
                }),
                choices,
                title: card_title(task, &row.title, &project, tier, &model),
                task: Some(task),
                asked_by: Some(row.supervisor),
                at,
            },
        )?;
        // The stuck notice sits in the Quartermaster's conversation.
        if let Some(seat) = qm_store::seat::quartermaster(tx)? {
            HeldService::anchor(tx, held, &task.to_string(), seat.id, at)?;
        }
    }
    task_store::event(
        tx,
        STUCK_EVENT,
        task,
        Some(agent),
        &format!("{task} is stuck: no report and no sign of life for 80 minutes"),
        serde_json::json!({ "turn": turn }),
        at,
    )?;
    let text = format!(
        "{task} is stuck; the user has a card with Continue, Drop task or Discard. Add what you \
         know with present_hold."
    );
    let wake = new_wake(
        row.supervisor,
        WakeKind::Watchdog,
        task,
        format!("watchdog:{task}:idle:{turn}:stuck"),
        &text,
        &format!("{task} is stuck"),
        serde_json::json!({}),
        at,
    );
    Inbox::post(tx, &wake, notify)?;
    Ok(())
}
