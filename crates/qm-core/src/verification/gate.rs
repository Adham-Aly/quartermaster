//! The report gate and the gate facts.
//!
//! [`VerificationService`] is the tasks' `ReportGate`: a done report on code work is refused while
//! the repair limit holds it, while a raised level's validation is missing, and (direct PR, unless
//! Careful before its review passed) until the checks on exactly the reported, pushed head passed
//! (`qm_policy::merge_gate::check_finish`). After the report it records the evidence manifest and
//! carries out the review's consequences ([`super::review`]). It is also the stuck notice's
//! `DecisionChoices`: the decision card's choices while a repair or review limit stands.

use super::VerificationService;
use super::evidence::{EVIDENCE_EVENT, manifest};
use super::levels::effective_of;
use super::repair::load_state;
use crate::inbox::Notify;
use crate::tasks::report::{GateNote, ReportGate};
use crate::watchdogs::DecisionChoices;
use jiff::Timestamp;
use qm_policy::holds::stuck_choices;
use qm_policy::merge_gate::{ChecksVerdict, TaskGate, check_finish};
use qm_policy::review::MAX_RE_REVIEWS;
use qm_store::StoreError;
use qm_store::reports;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::verification as store;
use qm_tools::defs::ReportArgs;
use qm_types::BoxFuture;
use qm_types::api::HeldChoice;
use qm_types::domain::{DeliveryMode, ReportOutcome, TaskKind, TaskStatus, VerificationLevel};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, ReportId, TaskId};
use rusqlite::Connection;

fn blank(s: Option<&String>) -> bool {
    s.is_none_or(|s| s.trim().is_empty())
}

/// The report has all five headings (Intent, What changed, Verified, Risk, Not done): the standard
/// heading check, which also counts as a raised level's validation.
pub fn headings_complete(args: &ReportArgs) -> bool {
    [
        &args.intent,
        &args.what_changed,
        &args.verified,
        &args.risk,
        &args.not_done,
    ]
    .iter()
    .all(|h| !blank(h.as_ref()))
}

/// The gate facts for `qm_policy::merge_gate` from the task's row.
pub fn task_gate(conn: &Connection, row: &TaskRow) -> Result<TaskGate, StoreError> {
    let facts = store::facts(conn, row.id)?;
    Ok(TaskGate {
        kind: row.kind,
        delivery: row.delivery,
        level: effective_of(row),
        review_passed: facts.as_ref().is_some_and(|f| f.review_passed_at.is_some()),
        validation_pending: facts.as_ref().is_some_and(|f| f.validation_pending),
        held: row.status == TaskStatus::Held || facts.as_ref().is_some_and(|f| f.repair.held),
    })
}

/// The refusal for a head that isn't pushed, or isn't the one the checks ran on.
const NOT_PUSHED: &str =
    "Your latest commit isn't pushed; push it with pr_open and wait for its checks";

impl VerificationService {
    /// The gate facts of `task`.
    pub fn gate(&self, conn: &Connection, task: TaskId) -> Result<Option<TaskGate>, StoreError> {
        task_store::load(conn, task)?
            .map(|row| task_gate(conn, &row))
            .transpose()
    }

    /// The gate for a done report on code work, on the database thread.
    fn check_done(
        &self,
        conn: &Connection,
        task: TaskId,
        report: &ReportArgs,
        head: Option<&CommitSha>,
    ) -> Result<Result<(), Refusal>, StoreError> {
        let Some(row) = task_store::load(conn, task)? else {
            return Ok(Ok(()));
        };
        if !row.kind.can_change_code() {
            return Ok(Ok(()));
        }
        // 1. The repair limit holds it.
        if let Err(r) = load_state(conn, task)?.check_finish() {
            return Ok(Err(r));
        }
        // 2. The gate, a raised level's validation counted as done by this report's headings.
        let mut gate = task_gate(conn, &row)?;
        if gate.validation_pending && headings_complete(report) {
            gate.validation_pending = false;
        }
        // 3. The checks verdict.
        let before_review = gate.level == Some(VerificationLevel::Reviewed) && !gate.review_passed;
        let verdict = if row.delivery == DeliveryMode::DirectPr && !before_review {
            let Some(ci) = self.ci().latest(conn, task)? else {
                return Ok(Err(Refusal::new(NOT_PUSHED)));
            };
            let on_head = head.is_some_and(|h| *h == ci.head)
                && ci.pushed_head.as_ref().is_some_and(|p| *p == ci.head);
            if !on_head {
                return Ok(Err(Refusal::new(NOT_PUSHED)));
            }
            if !ci.final_verdict {
                return Ok(Err(Refusal::new(format!(
                    "Checks are still running on {}; you'll be woken with the result",
                    ci.head.short()
                ))));
            }
            ci.verdict
        } else {
            ChecksVerdict::NoChecks
        };
        // 4. Finish.
        Ok(check_finish(
            &gate,
            &verdict,
            !blank(report.verified.as_ref()),
        ))
    }

    /// The consequences of a clear done report on code work.
    fn after_builder_report(
        &self,
        conn: &Connection,
        row: &TaskRow,
        report: &reports::ReportRow,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<GateNote, StoreError> {
        let task = row.id;
        let mut notes: Vec<String> = Vec::new();
        // A raised level's validation: a report that passes the standard heading check.
        let headings = |key: &str| {
            report
                .headings
                .get(key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
        };
        let complete = ["intent", "what_changed", "verified", "risk", "not_done"]
            .iter()
            .all(|k| headings(k));
        if complete {
            store::set_validation_pending(conn, task, false)?;
        }
        // The evidence manifest.
        let files = manifest(&self.data_dir().task(task));
        let count = files.len();
        let pieces = if count == 1 { "piece" } else { "pieces" };
        task_store::event(
            conn,
            EVIDENCE_EVENT,
            task,
            None,
            &format!("{task} reported {count} {pieces} of evidence"),
            serde_json::to_value(&files).unwrap_or_default(),
            at,
        )?;
        let level = effective_of(row);
        if count == 0
            && matches!(
                level,
                Some(VerificationLevel::Standard | VerificationLevel::Reviewed)
            )
        {
            notes.push(format!("No files in {task}'s evidence folder."));
        }
        // Careful: the review, or the re-review.
        if let Some(note) = Self::builder_reported(conn, row, notify, at)? {
            notes.push(note);
        }
        Ok(GateNote {
            note_for_supervisor: (!notes.is_empty()).then(|| notes.join(" ")),
        })
    }
}

impl ReportGate for VerificationService {
    fn check<'a>(
        &'a self,
        task: &'a TaskRow,
        report: &'a ReportArgs,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        self.check_with_head(task, report, None)
    }

    fn check_with_head<'a>(
        &'a self,
        task: &'a TaskRow,
        report: &'a ReportArgs,
        head: Option<&'a CommitSha>,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        let id = task.id;
        let report = report.clone();
        let head = head.cloned();
        Box::pin(async move {
            let svc = self.clone();
            match self
                .db()
                .call(move |conn| svc.check_done(conn, id, &report, head.as_ref()))
                .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    tracing::error!(task = %id, error = %e, "couldn't read the report gate's facts");
                    Err(Refusal::new(
                        "the app couldn't read this task's verification facts; try again",
                    ))
                }
            }
        })
    }

    fn after_report(
        &self,
        conn: &Connection,
        task: TaskId,
        report: ReportId,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<GateNote, StoreError> {
        let Some(row) = task_store::load(conn, task)? else {
            return Ok(GateNote::default());
        };
        let Some(stored) = reports::latest_clear(conn, task)?.filter(|r| r.id == report) else {
            return Ok(GateNote::default());
        };
        if stored.outcome != Some(ReportOutcome::Done) {
            return Ok(GateNote::default());
        }
        if row.kind == TaskKind::Review {
            return self.review_reported(conn, &row, &stored, notify, at);
        }
        if row.kind.can_change_code() {
            return self.after_builder_report(conn, &row, &stored, notify, at);
        }
        Ok(GateNote::default())
    }
}

impl DecisionChoices for VerificationService {
    fn stuck_choices<'a>(&'a self, task: &'a TaskRow) -> BoxFuture<'a, Vec<HeldChoice>> {
        let id = task.id;
        Box::pin(async move {
            let svc = self.clone();
            let at = self.clock().now();
            let decided = self
                .db()
                .call(move |conn| {
                    let facts = store::facts(conn, id)?;
                    let limit_stands = facts
                        .as_ref()
                        .is_some_and(|f| f.repair.held || f.review_rounds > MAX_RE_REVIEWS);
                    let hold = crate::decisions::hold::HoldFacts::load(conn, id, svc.health(), at)?;
                    Ok(match hold {
                        Some(h) => stuck_choices(limit_stands, h.tier, &h.usable, h.escalations),
                        None => stuck_choices(false, qm_types::domain::Tier::Medium, &[], 0),
                    })
                })
                .await;
            decided.unwrap_or_else(|e| {
                tracing::warn!(task = %id, error = %e, "couldn't read the stuck notice's facts");
                stuck_choices(false, qm_types::domain::Tier::Medium, &[], 0)
            })
        })
    }
}

/// Delivery's view of verification.
impl crate::delivery::ports::VerificationPort for VerificationService {
    fn gate(&self, conn: &Connection, task: TaskId) -> Result<TaskGate, StoreError> {
        VerificationService::gate(self, conn, task)?.ok_or_else(|| StoreError::Invalid {
            column: "tasks.id",
            message: format!("{task} doesn't exist"),
        })
    }

    fn before_push(
        &self,
        conn: &Connection,
        task: TaskId,
        failing_now: &[String],
        body_complete: bool,
    ) -> Result<Result<(), Refusal>, StoreError> {
        VerificationService::before_push(self, conn, task, failing_now, body_complete)
    }

    fn pushed(
        &self,
        conn: &Connection,
        task: TaskId,
        head: &CommitSha,
        failing_before: &[String],
        body_complete: bool,
    ) -> Result<(), StoreError> {
        VerificationService::pushed(self, conn, task, head, failing_before, body_complete)
    }

    fn checks_final(
        &self,
        conn: &Connection,
        task: TaskId,
        head: &CommitSha,
        failing: &[String],
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<crate::delivery::ports::ChecksFollowUp, StoreError> {
        VerificationService::checks_final(self, conn, task, head, failing, notify, at)
    }
}
