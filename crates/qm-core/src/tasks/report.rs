//! A crewmate's `report`.
//!
//! On a done report in a worktree, the task's own commits decide what it delivered: commits make it
//! code (their names are checked and the head recorded), none make it an answer. In place, the
//! headings decide. [`check_report`] then checks the shape against what was delivered, the kind,
//! level and delivery (pure); then the [`ReportGate`] (checks green in direct-PR mode, the review's
//! state, the repair limit). Then one transaction: the `reports` row, the status (`reported`, or
//! `waiting` for a decision), and the supervisor's wake. An answer is kept in its report row,
//! nowhere else.

use super::lifecycle::{TaskEvent, next_for};
use super::{TaskService, cut_chars, failed, new_wake, own_commits_failure, refuse};
use crate::inbox::Inbox;
use crate::inbox::Notify;
use jiff::Timestamp;
use qm_git::Repo;
use qm_policy::delivery::{Deliverable, DeliveredFacts, deliverable};
use qm_policy::names::{Boundary, check_commits};
use qm_store::StoreError;
use qm_store::reports::{self, NewReport};
use qm_store::tasks::{self as task_store, TaskRow};
use qm_tools::defs::{Ack, ReportArgs};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::BoxFuture;
use qm_types::domain::{
    DeliveryMode, ReportOutcome, ReportSource, TaskKind, TaskStatus, VerificationLevel, WakeKind,
};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, ModelId, ReportId, TaskId};
use rusqlite::Connection;
use std::fmt;
use std::fmt::Write as _;

/// The longest report text the supervisor's wake carries.
pub const WAKE_REPORT_CHARS: usize = 2_000;
/// `events.kind` of a report.
pub const REPORTED_EVENT: &str = "task.reported";

/// Verification's gate on a done report; the default passes everything.
pub trait ReportGate: Send + Sync + fmt::Debug {
    fn check<'a>(
        &'a self,
        task: &'a TaskRow,
        report: &'a ReportArgs,
    ) -> BoxFuture<'a, Result<(), Refusal>>;

    /// [`Self::check`] with the head commit the report delivers; by default the head isn't needed.
    fn check_with_head<'a>(
        &'a self,
        task: &'a TaskRow,
        report: &'a ReportArgs,
        head: Option<&'a CommitSha>,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        let _ = head;
        self.check(task, report)
    }

    /// Inside the report's transaction, after its row and status are written, for every clear
    /// report: verification's consequences (the review's verdict, the re-review, the evidence
    /// manifest). Its note is appended to the supervisor's wake.
    fn after_report(
        &self,
        conn: &Connection,
        task: TaskId,
        report: ReportId,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<GateNote, StoreError> {
        let _ = (conn, task, report, notify, at);
        Ok(GateNote::default())
    }
}

/// What [`ReportGate::after_report`] adds to the supervisor's `reported` wake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GateNote {
    pub note_for_supervisor: Option<String>,
}

/// The gate that passes every report.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenGate;

impl ReportGate for OpenGate {
    fn check<'a>(
        &'a self,
        task: &'a TaskRow,
        report: &'a ReportArgs,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        let _ = (task, report);
        Box::pin(async { Ok(()) })
    }
}

fn blank(s: Option<&String>) -> bool {
    s.is_none_or(|s| s.trim().is_empty())
}

/// The report's shape against the task and what it delivered (pure). The kind only adds its own
/// parts: a review's verdict, a reproduction's `reproduced`.
pub fn check_report(
    kind: TaskKind,
    level: Option<VerificationLevel>,
    delivery: DeliveryMode,
    delivered: Deliverable,
    args: &ReportArgs,
) -> Result<(), Refusal> {
    if args.review.is_some() && kind != TaskKind::Review {
        return Err(Refusal::new(
            "only a review task reports a review; leave review out",
        ));
    }
    match args.outcome {
        ReportOutcome::Done => {
            if kind == TaskKind::Reproduce {
                check_reproduced(args)?;
            }
            if kind == TaskKind::Review && args.review.is_none() {
                return Err(Refusal::new(
                    "a review's done report carries its verdict, findings and risk in review",
                ));
            }
            match delivered {
                Deliverable::Code => check_headings(level, delivery, args)?,
                Deliverable::Answer => {
                    if blank(args.answer.as_ref()) {
                        let why = if delivery == DeliveryMode::Folder {
                            "a done report carries the answer, or the five headings if you changed files"
                        } else {
                            "your branch has no commits of its own, so this done report delivers an answer: carry it in answer, or commit your work first"
                        };
                        return Err(Refusal::new(why));
                    }
                }
            }
        }
        ReportOutcome::Blocked => {}
        ReportOutcome::NeedsDecision => {
            if blank(args.intent.as_ref())
                && blank(args.not_done.as_ref())
                && args.blockers.iter().all(|b| b.trim().is_empty())
            {
                return Err(Refusal::new(
                    "a needs_decision report says what needs deciding, in blockers or Not done",
                ));
            }
        }
    }
    Ok(())
}

/// A done report that delivers code: all five headings at standard and reviewed.
fn check_headings(
    level: Option<VerificationLevel>,
    delivery: DeliveryMode,
    args: &ReportArgs,
) -> Result<(), Refusal> {
    let headings_required = delivery != DeliveryMode::BuildInPlace
        && matches!(
            level,
            Some(VerificationLevel::Standard | VerificationLevel::Reviewed)
        );
    if !headings_required {
        return Ok(());
    }
    let missing: Vec<&str> = [
        (&args.intent, "Intent"),
        (&args.what_changed, "What changed"),
        (&args.verified, "Verified"),
        (&args.risk, "Risk"),
        (&args.not_done, "Not done"),
    ]
    .into_iter()
    .filter(|(v, _)| blank(v.as_ref()))
    .map(|(_, name)| name)
    .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let level = level.map_or("standard", VerificationLevel::as_str);
    Err(Refusal::new(format!(
        "A done report that delivers code at the {level} level needs all five headings; missing: {}",
        missing.join(", ")
    )))
}

/// A reproduction's done report says `reproduced`, and its answer opens with the matching line.
fn check_reproduced(args: &ReportArgs) -> Result<(), Refusal> {
    let Some(reproduced) = args.reproduced else {
        return Err(Refusal::new(
            "a reproduction's done report says reproduced: true or false",
        ));
    };
    let first = args
        .answer
        .as_deref()
        .and_then(|a| a.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let expected = if reproduced {
        "reproduced: yes"
    } else {
        "reproduced: no"
    };
    if first != expected {
        return Err(Refusal::new(format!(
            "a reproduction's answer opens with the line \"{expected}\", matching reproduced"
        )));
    }
    Ok(())
}

/// The headings as `reports.headings` keeps them.
fn headings(args: &ReportArgs) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in [
        ("intent", &args.intent),
        ("what_changed", &args.what_changed),
        ("verified", &args.verified),
        ("risk", &args.risk),
        ("not_done", &args.not_done),
    ] {
        if let Some(v) = value.as_ref().filter(|v| !v.trim().is_empty()) {
            map.insert(
                key.to_owned(),
                serde_json::Value::String(v.trim().to_owned()),
            );
        }
    }
    serde_json::Value::Object(map)
}

/// The supervisor's wake text: the report, compact, cut at [`WAKE_REPORT_CHARS`].
fn wake_text(task: TaskId, row: &TaskRow, args: &ReportArgs) -> String {
    let mut text = format!(
        "{task} reported {} ({}): {}.",
        args.outcome, row.kind, row.title
    );
    for (name, value) in [
        ("Intent", &args.intent),
        ("What changed", &args.what_changed),
        ("Verified", &args.verified),
        ("Risk", &args.risk),
        ("Not done", &args.not_done),
    ] {
        if let Some(v) = value.as_ref().filter(|v| !v.trim().is_empty()) {
            let _ = write!(text, " {name}: {}", v.trim());
        }
    }
    if let Some(reproduced) = args.reproduced {
        let _ = write!(
            text,
            " Reproduced: {}.",
            if reproduced { "yes" } else { "no" }
        );
    }
    if let Some(review) = &args.review {
        let _ = write!(
            text,
            " Verdict: {}; risk {} ({}); {} findings.",
            review.verdict,
            review.risk,
            review.risk_reason.trim(),
            review.findings.len()
        );
    }
    if let Some(answer) = args.answer.as_ref().filter(|a| !a.trim().is_empty()) {
        let _ = write!(text, " Answer: {}", answer.trim());
    }
    if !args.blockers.is_empty() {
        let _ = write!(text, " Blockers: {}", args.blockers.join("; "));
    }
    cut_chars(
        &text,
        WAKE_REPORT_CHARS,
        &format!("… read it all with task_status {task}"),
    )
}

/// The caller's task, for a crewmate tool.
pub(crate) async fn callers_task(
    svc: &TaskService,
    caller: &Caller,
) -> Result<TaskRow, ToolFailure> {
    let Some(task) = caller.task else {
        return Err(refuse("this tool is for a crewmate working on a task"));
    };
    svc.db()
        .call(move |conn| task_store::load(conn, task))
        .await
        .map_err(|e| failed(&e))?
        .ok_or_else(|| refuse(format!("{task} doesn't exist")))
}

/// Whether a done report must have the worktree's own commits read: every done report in a git
/// mode, whatever the kind, since commits are what make it code. A folder task has no commits; its
/// edits in the folder are the delivery.
pub(crate) fn needs_own_commits(outcome: ReportOutcome, delivery: DeliveryMode) -> bool {
    outcome == ReportOutcome::Done && delivery != DeliveryMode::Folder
}

/// Why a re-report can't be taken now: the task is settling and its approval is no longer pending,
/// so a merge or push of the reported head is under way. `None` otherwise.
fn settle_under_way(row: &TaskRow, pending_holds: usize) -> Option<String> {
    (row.status == TaskStatus::Settling && pending_holds == 0).then(|| {
        format!(
            "{} is being delivered now, so a new report can't change what lands. End your turn; \
             your supervisor hears the outcome and steers you if there's more to do",
            row.id
        )
    })
}

/// `report`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn record(
    svc: &TaskService,
    caller: &Caller,
    args: ReportArgs,
) -> Result<Ack, ToolFailure> {
    let row = callers_task(svc, caller).await?;
    let task = row.id;
    // A plan task: `plan.md` is checked for the live mode, and a round under normal autonomy goes
    // to the user (waiting on them, no supervisor wake) rather than to the supervisor.
    let plan = if row.kind == TaskKind::Plan {
        super::plan::before_report(svc, caller, args.outcome).await?
    } else {
        None
    };
    let to_user = plan
        .as_ref()
        .is_some_and(|p| p.to_user && p.state.stage != crate::plans::Stage::Approved);
    let event = match args.outcome {
        _ if to_user => TaskEvent::NeedsDecision,
        ReportOutcome::Done | ReportOutcome::Blocked => TaskEvent::Reported,
        ReportOutcome::NeedsDecision => TaskEvent::NeedsDecision,
    };
    let pending_holds = if row.status == TaskStatus::Settling {
        svc.db()
            .call(move |conn| qm_store::delivery::pending_settle_holds(conn, task))
            .await
            .map_err(|e| failed(&e))?
            .len()
    } else {
        0
    };
    if let Some(why) = settle_under_way(&row, pending_holds) {
        return Err(refuse(why));
    }
    if let Some(why) = svc
        .db()
        .call(move |conn| {
            crate::jobs::helpers_running(conn, task, crate::jobs::HelperWait::Crewmate("report"))
        })
        .await
        .map_err(|e| failed(&e))?
    {
        return Err(refuse(why));
    }
    let to = next_for(task, row.status, event)?;

    // On done in a git mode: the task's own commits. Commits make the report code (their names
    // are checked and the head recorded); none make it an answer. A folder task has no commits:
    // its headings decide.
    let mut head: Option<CommitSha> = None;
    if needs_own_commits(args.outcome, row.delivery) {
        let paths = svc.paths_of(&row);
        let project = svc
            .db()
            .call(move |conn| super::load_project(conn, row.project))
            .await
            .map_err(|e| failed(&e))?
            .ok_or_else(|| refuse(format!("{task}'s project is gone")))?;
        let git = svc.deps().git.clone();
        let branch = row
            .base_branch
            .clone()
            .unwrap_or_else(|| project.default_branch.clone());
        let worktree = paths.worktree.clone();
        let checkout = project.path.clone();
        let (messages, worktree_head) = tokio::task::spawn_blocking(move || {
            let repo = Repo::open(git, &checkout)?;
            let Some(head) = repo.worktree_head(&worktree)? else {
                return Ok((Vec::new(), None));
            };
            let own = repo.own_commits(&head, &branch)?;
            Ok::<_, qm_git::GitError>((own.commits, Some(head)))
        })
        .await
        .map_err(|e| failed(&e))?
        .map_err(|e| own_commits_failure(&e))?;
        if !messages.is_empty() {
            let models: Vec<ModelId> = svc
                .db()
                .call(|conn| qm_store::models::pool(conn))
                .await
                .map_err(|e| failed(&e))?
                .into_iter()
                .flat_map(|p| [p.facts.model, p.facts.canonical])
                .collect();
            check_commits(
                messages.iter().map(|(_, m)| m.as_str()),
                &models,
                Boundary::DoneReport,
            )?;
            head = worktree_head;
        }
    }
    let delivered = deliverable(DeliveredFacts {
        in_place: row.delivery == DeliveryMode::Folder,
        own_commits: head.is_some(),
        headings: crate::verification::gate::any_heading(&args),
    });
    check_report(
        row.kind,
        row.verification.map(|(l, _)| l),
        row.delivery,
        delivered,
        &args,
    )?;
    if args.outcome == ReportOutcome::Done && crate::reports::required(row.kind) {
        crate::reports::check(&svc.paths_of(&row)).map_err(|e| {
            refuse(format!(
                "Write $QM_REPORT_DIR/report.html using template.html and SKILL.md before reporting done: {e}"
            ))
        })?;
    }
    if args.outcome == ReportOutcome::Done {
        svc.deps()
            .gate
            .check_with_head(&row, &args, head.as_ref())
            .await?;
    }

    let at = svc.clock().now();
    let agent = caller.agent;
    let mut text = wake_text(task, &row, &args);
    if let Some(plan) = plan
        .as_ref()
        .filter(|p| p.state.stage == crate::plans::Stage::Approved)
    {
        text.push(' ');
        text.push_str(&super::plan::ready_text(
            task,
            &row,
            match plan.mode {
                qm_types::domain::PlanMode::Internal => "is finished under auto autonomy",
                qm_types::domain::PlanMode::Interactive => {
                    "is final and approved, with the planner's recommendations"
                }
            },
        ));
    }
    // A re-report replaces the report that stood.
    let again = matches!(row.status, TaskStatus::Reported | TaskStatus::Settling);
    if again {
        let at_head = head
            .as_ref()
            .map(|h| format!(" at {}", h.short()))
            .unwrap_or_default();
        text = format!("{task} reported again{at_head}, replacing its earlier report. {text}");
    }
    let gate = std::sync::Arc::clone(&svc.deps().gate);
    let from = row.status;
    let supervisor = row.supervisor;
    let outcome = args.outcome;
    let ack = if to_user {
        "Reported. The user has your plan's page; end your turn and wait for their reply."
    } else if outcome == ReportOutcome::NeedsDecision {
        "Reported. Your supervisor has it; wait for its decision."
    } else {
        "Reported. Your supervisor has it."
    };
    let round = plan.as_ref().map_or(0, |p| p.state.round);
    svc.write(move |tx, notify| {
        let review = args.review.as_ref();
        let report = reports::insert(
            tx,
            &NewReport {
                task,
                agent,
                source: ReportSource::ReportCall,
                outcome: Some(outcome),
                headings: headings(&args),
                answer: args.answer.as_deref(),
                reproduced: args.reproduced,
                diagnosis: args.diagnosis,
                review_verdict: review.map(|r| r.verdict),
                review_findings: review.map_or_else(
                    || serde_json::json!([]),
                    |r| serde_json::to_value(&r.findings).unwrap_or_default(),
                ),
                review_risk: review.map(|r| r.risk),
                review_risk_reason: review.map(|r| r.risk_reason.as_str()),
                blockers: serde_json::to_value(&args.blockers).unwrap_or_default(),
                head_commit: head.as_ref(),
                final_text: None,
                at,
            },
        )?;
        if let Some(head) = &head {
            task_store::set_head_commit(tx, task, head)?;
        }
        if again {
            // Cards are tied to the head they were made for: a re-report voids them.
            let why = head.as_ref().map_or_else(
                || "the crewmate reported again".to_owned(),
                |h| format!("the crewmate reported again at {}", h.short()),
            );
            let mut voided = false;
            for held in qm_store::delivery::pending_settle_holds(tx, task)? {
                if crate::held::HeldService::supersede_with(tx, held, Some(task), &why, at)? {
                    voided = true;
                    task_store::event(
                        tx,
                        crate::delivery::pr::SUPERSEDED_EVENT,
                        task,
                        None,
                        &format!("{task}'s approval no longer applies: {why}"),
                        serde_json::json!({ "held": held, "head": head }),
                        at,
                    )?;
                }
            }
            if voided {
                text.push_str(" The approval that waited on the earlier report no longer applies.");
            }
        }
        task_store::set_status(tx, task, from, to, at)?;
        if to_user {
            task_store::set_waiting_on(tx, task, Some(&serde_json::json!({ "plan": round })))?;
            task_store::event(
                tx,
                super::plan::ROUND_EVENT,
                task,
                Some(agent),
                &format!("{task}'s plan round {round} went to the user"),
                serde_json::json!({ "report": report, "round": round }),
                at,
            )?;
        } else if to == TaskStatus::Waiting {
            task_store::set_waiting_on(
                tx,
                task,
                Some(&serde_json::json!({ "decision": report.get() })),
            )?;
        }
        task_store::event(
            tx,
            REPORTED_EVENT,
            task,
            Some(agent),
            &format!("{task} reported {outcome}"),
            serde_json::json!({ "report": report, "outcome": outcome }),
            at,
        )?;
        if let Some(note) = gate
            .after_report(tx, task, report, notify, at)?
            .note_for_supervisor
        {
            text.push(' ');
            text.push_str(&note);
        }
        if to_user {
            // The round is the user's: the app posts its card, and the supervisor isn't woken.
            return Ok(());
        }
        let (kind, key, short) = if outcome == ReportOutcome::NeedsDecision {
            (
                WakeKind::DecisionAsked,
                format!("decision:{task}:report-{report}"),
                format!("{task} needs a decision"),
            )
        } else {
            (
                WakeKind::Reported,
                format!("reported:{task}:{report}"),
                format!("{task} reported {outcome}"),
            )
        };
        let wake = new_wake(
            supervisor,
            kind,
            task,
            key,
            &text,
            &short,
            serde_json::json!({ "report": report }),
            at,
        );
        Inbox::post(tx, &wake, notify)?;
        if to == TaskStatus::Reported {
            super::waits::release_waiters(tx, notify, task, TaskStatus::Reported, at)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| failed(&e))?;
    svc.tell_outside(task, true);
    if let Some(plan) = plan {
        super::plan::after_report(svc, task, plan).await;
    }
    Ok(Ack {
        note: ack.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done() -> ReportArgs {
        ReportArgs {
            outcome: ReportOutcome::Done,
            intent: None,
            what_changed: None,
            verified: None,
            risk: None,
            not_done: None,
            answer: None,
            reproduced: None,
            diagnosis: false,
            review: None,
            blockers: Vec::new(),
        }
    }

    fn all_headings() -> ReportArgs {
        let h = || Some("x".to_owned());
        ReportArgs {
            intent: h(),
            what_changed: h(),
            verified: h(),
            risk: h(),
            not_done: h(),
            ..done()
        }
    }

    const STANDARD: Option<VerificationLevel> = Some(VerificationLevel::Standard);

    #[test]
    fn what_was_delivered_decides_the_shape_not_the_kind() {
        let pr = DeliveryMode::DirectPr;
        // A research task that committed reports like code.
        assert!(
            check_report(
                TaskKind::Research,
                STANDARD,
                pr,
                Deliverable::Code,
                &all_headings()
            )
            .is_ok()
        );
        assert!(
            check_report(TaskKind::Research, STANDARD, pr, Deliverable::Code, &done()).is_err()
        );
        // A build task with no commits delivers an answer.
        let answer = ReportArgs {
            answer: Some("it's in the docs".to_owned()),
            ..done()
        };
        assert!(check_report(TaskKind::Build, STANDARD, pr, Deliverable::Answer, &answer).is_ok());
        let refusal = check_report(TaskKind::Build, STANDARD, pr, Deliverable::Answer, &done())
            .expect_err("no answer");
        assert!(
            refusal.to_string().contains("commit your work"),
            "{refusal}"
        );
    }

    #[test]
    fn any_kind_may_mark_a_diagnosis_or_reproduced_but_only_reviews_report_a_review() {
        let pr = DeliveryMode::DirectPr;
        let marked = ReportArgs {
            answer: Some("the cause".to_owned()),
            diagnosis: true,
            reproduced: Some(true),
            ..done()
        };
        for kind in [TaskKind::Lookup, TaskKind::Fix, TaskKind::Research] {
            assert!(check_report(kind, STANDARD, pr, Deliverable::Answer, &marked).is_ok());
        }
        // A reproduction still needs reproduced, matching its answer's first line.
        let plain = ReportArgs {
            answer: Some("reproduced: yes".to_owned()),
            ..done()
        };
        assert!(
            check_report(
                TaskKind::Reproduce,
                STANDARD,
                pr,
                Deliverable::Answer,
                &plain
            )
            .is_err()
        );
        let review = ReportArgs {
            answer: Some("fine".to_owned()),
            review: Some(qm_tools::defs::ReviewReport {
                verdict: qm_types::domain::ReviewVerdict::Pass,
                findings: Vec::new(),
                risk: qm_types::domain::RiskLevel::Low,
                risk_reason: "small".to_owned(),
            }),
            ..done()
        };
        assert!(check_report(TaskKind::Build, STANDARD, pr, Deliverable::Answer, &review).is_err());
        assert!(check_report(TaskKind::Review, STANDARD, pr, Deliverable::Answer, &review).is_ok());
    }
}
