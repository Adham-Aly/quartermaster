//! Failed checks and repair counting.
//!
//! `qm_policy::repair::RepairState` is loaded from the task's row and saved back in the transaction
//! of the act that changed it ([`load_state`], [`save_state`]). The failed-check rules are
//! Default's (and so Careful's); at `quick` nothing is required before a push, nothing is counted
//! and the repair limit never holds. `ci_rerun` keeps its own rule at every level: once per task,
//! after a flake diagnosis with evidence.
//!
//! Delivery calls [`VerificationService::before_push`], [`VerificationService::pushed`] and
//! [`VerificationService::checks_final`] from its own transactions.

use super::VerificationService;
use super::levels::effective_of;
use crate::decisions::hold::{HoldFacts, hold};
use crate::held::HeldService;
use crate::inbox::Notify;
use jiff::Timestamp;
use qm_policy::repair::{MAX_FAILED_REPAIRS, RepairOutcome, RepairState};
use qm_store::StoreError;
use qm_store::held::HoldReason;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::verification::{self as store, RepairRow};
use qm_tools::defs::{Ack, CiDiagnosisArgs};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{CiDiagnosis, VerificationLevel};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// `events.kind` of a declared diagnosis.
pub const DIAGNOSIS_EVENT: &str = "task.diagnosis";
/// `events.kind` of a repair that didn't fix its check.
pub const REPAIR_FAILED_EVENT: &str = "task.repair_failed";
/// `events.kind` of a repair limit that couldn't hold its task (shown in the context block).
pub const HOLD_FAILED_EVENT: &str = "task.hold_failed";
/// The held kinds a repair hold supersedes on a settling task.
const SETTLE_KINDS: &[&str] = &["merge", "push"];
/// The longest evidence `ci_diagnosis` takes (checked by the handler).
pub use qm_tools::handlers::verification::MAX_EVIDENCE_CHARS;

/// `repair_pending`'s JSON: the diagnoses declared since the last push and the checks the last
/// push meant to fix.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pending {
    #[serde(default)]
    declared: Vec<qm_policy::repair::Declared>,
    #[serde(default)]
    awaiting: Vec<String>,
}

fn from_row(row: &RepairRow) -> RepairState {
    let pending: Pending = serde_json::from_str(&row.pending).unwrap_or_default();
    RepairState {
        declared: pending.declared,
        awaiting: pending.awaiting,
        failed_repairs: row.attempts,
        rerun_used: row.rerun_used,
        held: row.held,
    }
}

fn to_row(state: &RepairState) -> Result<RepairRow, StoreError> {
    let pending = Pending {
        declared: state.declared.clone(),
        awaiting: state.awaiting.clone(),
    };
    Ok(RepairRow {
        pending: serde_json::to_string(&pending).map_err(|source| StoreError::Json {
            column: "tasks.repair_pending",
            source,
        })?,
        attempts: state.failed_repairs,
        rerun_used: state.rerun_used,
        held: state.held,
    })
}

/// The task's repair counters.
pub fn load_state(conn: &Connection, task: TaskId) -> Result<RepairState, StoreError> {
    Ok(store::facts(conn, task)?
        .map(|f| from_row(&f.repair))
        .unwrap_or_default())
}

/// Saves the task's repair counters, in the caller's transaction.
pub fn save_state(conn: &Connection, task: TaskId, state: &RepairState) -> Result<(), StoreError> {
    store::save_repair(conn, task, &to_row(state)?)
}

/// Whether the failed-check rules apply to `row` (Default and Careful; not Quick, not tasks with
/// no level).
pub fn rules_apply(row: &TaskRow) -> bool {
    matches!(
        effective_of(row),
        Some(VerificationLevel::Standard | VerificationLevel::Reviewed)
    )
}

/// The diagnosis in the words of the event: "own regression".
fn diagnosis_words(d: CiDiagnosis) -> &'static str {
    match d {
        CiDiagnosis::OwnRegression => "own regression",
        CiDiagnosis::SuspectedFlake => "suspected flake",
        CiDiagnosis::FailingOnMain => "already failing on main",
    }
}

pub use crate::delivery::ports::ChecksFollowUp;

/// The caller's task, for a crewmate tool.
async fn callers_row(svc: &VerificationService, caller: &Caller) -> Result<TaskRow, ToolFailure> {
    let Some(task) = caller.task else {
        return Err(Refusal::new("this tool is for a crewmate working on a task").into());
    };
    svc.db()
        .call(move |conn| task_store::load(conn, task))
        .await
        .map_err(|e| ToolFailure::Failed(e.to_string()))?
        .ok_or_else(|| Refusal::new(format!("{task} doesn't exist")).into())
}

impl VerificationService {
    /// `ci_diagnosis`, after the handler's checks: free to call, for a check that is failing on the
    /// pushed head.
    pub async fn declare_diagnosis(
        &self,
        caller: &Caller,
        args: CiDiagnosisArgs,
    ) -> Result<Ack, ToolFailure> {
        let row = callers_row(self, caller).await?;
        let task = row.id;
        let agent = caller.agent;
        let ci = self.ci();
        let at = self.clock().now();
        let check = args.check.trim().to_owned();
        let diagnosis = args.diagnosis;
        let evidence = args
            .evidence
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty());
        let has_evidence = evidence.is_some();
        let evidence = evidence.map(qm_policy::markers::neutralise);
        let outcome = self
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                let latest = ci.latest(&tx, task)?;
                let failing = latest
                    .as_ref()
                    .filter(|f| f.final_verdict)
                    .is_some_and(|f| f.failing.contains(&check));
                if !failing {
                    let head = latest
                        .as_ref()
                        .map(|f| f.pushed_head.as_ref().unwrap_or(&f.head).short().to_owned());
                    let reason = match head {
                        Some(head) => format!(
                            "check {check} isn't failing on your latest pushed commit {head}"
                        ),
                        None => format!(
                            "check {check} isn't failing on your latest pushed commit (nothing is pushed yet)"
                        ),
                    };
                    return Ok(Err(Refusal::new(reason)));
                }
                let mut state = load_state(&tx, task)?;
                if let Err(r) = state.declare(&check, diagnosis, has_evidence) {
                    return Ok(Err(r));
                }
                save_state(&tx, task, &state)?;
                task_store::event(
                    &tx,
                    DIAGNOSIS_EVENT,
                    task,
                    Some(agent),
                    &format!("{task} diagnosed {check}: {}", diagnosis_words(diagnosis)),
                    serde_json::json!({
                        "check": check,
                        "diagnosis": diagnosis,
                        "evidence": evidence,
                    }),
                    at,
                )?;
                tx.commit()?;
                let note = match diagnosis {
                    CiDiagnosis::OwnRegression => format!(
                        "Noted. If {check} fails again after your next push, that uses a repair \
                         attempt ({} of {MAX_FAILED_REPAIRS} used).",
                        state.failed_repairs
                    ),
                    CiDiagnosis::SuspectedFlake => {
                        "Noted. You may rerun it once for this task with ci_rerun.".to_owned()
                    }
                    CiDiagnosis::FailingOnMain => "Noted. File the separate task with \
                         file_followup (your reproduction on an untouched main as its evidence) \
                         and wait on it with declare_wait; waiting costs nothing."
                        .to_owned(),
                };
                Ok(Ok(note))
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        let note = outcome?;
        self.notify(&Notify::new());
        Ok(Ack { note })
    }

    /// `ci_rerun`: once per task, after a flake diagnosis with evidence; used only once GitHub
    /// accepted the rerun.
    pub async fn rerun_check(&self, caller: &Caller, check: String) -> Result<Ack, ToolFailure> {
        let row = callers_row(self, caller).await?;
        let task = row.id;
        let check = check.trim().to_owned();
        let probe = check.clone();
        let decided = self
            .db()
            .call(move |conn| {
                let mut state = load_state(conn, task)?;
                Ok(state.rerun(&probe))
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        decided?;
        self.ci().rerun(task, &check).await?;
        // Delivery recorded the rerun's `task.rerun` event; the once-per-task rule is used now.
        self.db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                let mut state = load_state(&tx, task)?;
                state.rerun_used = true;
                save_state(&tx, task, &state)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        self.notify(&Notify::new());
        Ok(Ack {
            note: format!(
                "Rerunning {check} on your pushed commit. You'll be woken with the result; this \
                 task can't rerun checks again."
            ),
        })
    }

    /// Before `pr_open` (or a branch-only re-settle) pushes: a held task can't push; at the
    /// failed-check levels every failing check needs a diagnosis first; a raised level's missing
    /// validation needs a complete body. Only a check: the diagnoses are used up when the push is
    /// recorded ([`Self::pushed`]), so a push that fails keeps them.
    pub fn before_push(
        &self,
        conn: &Connection,
        task: TaskId,
        failing_now: &[String],
        body_complete: bool,
    ) -> Result<Result<(), Refusal>, StoreError> {
        let Some(row) = task_store::load(conn, task)? else {
            return Ok(Err(Refusal::new(format!("{task} doesn't exist"))));
        };
        let Some(facts) = store::facts(conn, task)? else {
            return Ok(Err(Refusal::new(format!("{task} doesn't exist"))));
        };
        if facts.repair.held {
            return Ok(Err(Refusal::new(
                "the task is held after 3 failed repairs; the decision is with the user",
            )));
        }
        if facts.validation_pending && !body_complete {
            return Ok(Err(Refusal::new(
                "the verification level was raised: do the standard validation and put it in the \
                 PR body (Intent, What changed, Verified, Risk and Not done) before pushing",
            )));
        }
        if !rules_apply(&row) {
            return Ok(Ok(()));
        }
        let mut state = from_row(&facts.repair);
        let failing: Vec<&str> = failing_now.iter().map(String::as_str).collect();
        Ok(state.on_push(&failing))
    }

    /// In the push's transaction: a complete PR body counts as the raised level's validation, and
    /// at the failed-check levels the diagnoses declared for the checks failing before the push are
    /// used up, the own regressions among them awaited on the new head.
    pub fn pushed(
        &self,
        conn: &Connection,
        task: TaskId,
        head: &CommitSha,
        failing_before: &[String],
        body_complete: bool,
    ) -> Result<(), StoreError> {
        if body_complete {
            store::set_validation_pending(conn, task, false)?;
        }
        let Some(row) = task_store::load(conn, task)? else {
            return Ok(());
        };
        if !rules_apply(&row) {
            return Ok(());
        }
        let mut state = load_state(conn, task)?;
        let failing: Vec<&str> = failing_before.iter().map(String::as_str).collect();
        match state.on_push(&failing) {
            Ok(()) => save_state(conn, task, &state),
            Err(r) => {
                // Checked before the push; the push happened anyway, so it stands.
                tracing::warn!(%task, %head, reason = %r.reason, "a push was recorded that the repair rule no longer allows");
                Ok(())
            }
        }
    }

    /// In a final verdict's transaction: repair counting, and the hold on the third failed repair.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn checks_final(
        &self,
        conn: &Connection,
        task: TaskId,
        head: &CommitSha,
        failing: &[String],
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<ChecksFollowUp, StoreError> {
        let Some(row) = task_store::load(conn, task)? else {
            return Ok(ChecksFollowUp::default());
        };
        if !rules_apply(&row) {
            return Ok(ChecksFollowUp::default());
        }
        let mut state = load_state(conn, task)?;
        let awaited = state.awaiting.clone();
        let failing_refs: Vec<&str> = failing.iter().map(String::as_str).collect();
        let outcome = state.on_results(&failing_refs);
        save_state(conn, task, &state)?;
        let again: Vec<&str> = awaited
            .iter()
            .map(String::as_str)
            .filter(|c| failing_refs.contains(c))
            .collect();
        Ok(match outcome {
            RepairOutcome::Nothing => ChecksFollowUp::default(),
            RepairOutcome::Counted { used } => {
                task_store::event(
                    conn,
                    REPAIR_FAILED_EVENT,
                    task,
                    None,
                    &format!(
                        "{task}: repair {used} of {MAX_FAILED_REPAIRS} didn't fix {}",
                        again.join(", ")
                    ),
                    serde_json::json!({ "used": used, "checks": again, "head": head }),
                    at,
                )?;
                ChecksFollowUp {
                    note: Some(format!(
                        "Repair attempts used: {used} of {MAX_FAILED_REPAIRS}."
                    )),
                    held: false,
                }
            }
            RepairOutcome::Hold => {
                task_store::event(
                    conn,
                    REPAIR_FAILED_EVENT,
                    task,
                    None,
                    &format!(
                        "{task}: repair {MAX_FAILED_REPAIRS} of {MAX_FAILED_REPAIRS} didn't fix {}",
                        again.join(", ")
                    ),
                    serde_json::json!({
                        "used": MAX_FAILED_REPAIRS,
                        "checks": again,
                        "head": head,
                    }),
                    at,
                )?;
                let detail = format!(
                    "{} still failed after each fix (last on {}).",
                    again.join(", "),
                    head.short()
                );
                // A task waiting in `settling` (branch-only, for its pushed branch's checks) is
                // held too; its pending settle approval no longer applies.
                if row.status == qm_types::domain::TaskStatus::Settling {
                    for held in store::pending_held_of(conn, task, SETTLE_KINDS)? {
                        HeldService::supersede_with(
                            conn,
                            held,
                            Some(task),
                            "the task is held after 3 failed repairs",
                            at,
                        )?;
                    }
                }
                let refused = match HoldFacts::load(conn, task, self.health(), at)? {
                    Some(facts) => hold(
                        conn,
                        task,
                        HoldReason::RepairLimit,
                        &detail,
                        &facts,
                        notify,
                        at,
                    )?
                    .err(),
                    None => Some(Refusal::new(format!("{task} doesn't exist"))),
                };
                match refused {
                    None => ChecksFollowUp {
                        note: Some(
                            "The app is holding this task after 3 failed repairs; the user will \
                             decide."
                                .to_owned(),
                        ),
                        held: true,
                    },
                    Some(why) => {
                        // Never a silent half-hold: without a decision card the limit doesn't
                        // stand, and the crewmate isn't told the user will decide.
                        tracing::warn!(%task, reason = %why.reason, "the repair limit couldn't hold the task");
                        state.held = false;
                        save_state(conn, task, &state)?;
                        task_store::event(
                            conn,
                            HOLD_FAILED_EVENT,
                            task,
                            None,
                            &format!(
                                "{task} reached {MAX_FAILED_REPAIRS} failed repairs but couldn't be held: {}",
                                why.reason
                            ),
                            serde_json::json!({ "reason": why.reason }),
                            at,
                        )?;
                        ChecksFollowUp {
                            note: Some(format!(
                                "Repair attempts used: {MAX_FAILED_REPAIRS} of {MAX_FAILED_REPAIRS}."
                            )),
                            held: false,
                        }
                    }
                }
            }
        })
    }
}
