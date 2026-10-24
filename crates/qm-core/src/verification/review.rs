//! The Careful review.
//!
//! | Step | Who |
//! |---|---|
//! | 1 The builder commits locally and reports | the builder; `check_finish` accepts it without CI |
//! | 2 A separate review task starts | the supervisor (prompted by the `reported` wake's note); [`VerificationService`] as the `StartHook` moves the builder `in_review` |
//! | 3–4 The reviewer checks everything and reports a verdict | the reviewer; `qm_policy::review::outcome` reads it |
//! | 5 Auto-fix findings go to the builder word for word | the supervisor, prompted by the note |
//! | 6 The re-review of the new head | the app (`VerificationService::builder_reported`) |
//! | 7 The third "findings" verdict holds | the app (`decisions::hold`) |
//! | 8 A pass releases the hold | the app; the builder pushes and gets CI green |
//!
//! The review's risk is recorded, never blocking. A review of a task that isn't Careful is only an
//! answer: it gates nothing.

use super::evidence::{brief_lines, manifest};
use super::levels::effective_of;
use super::{VerificationService, wake_crewmate};
use crate::decisions::hold::{HoldFacts, hold};
use crate::inbox::Notify;
use crate::tasks::lifecycle::{TaskEvent, next_for};
use crate::tasks::report::GateNote;
use crate::tasks::start::StartHook;
use jiff::Timestamp;
use qm_policy::review::{ReviewOutcome, outcome};
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::held::HoldReason;
use qm_store::reports::{self, ReportRow};
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::verification as store;
use qm_tools::defs::Finding;
use qm_types::data_dir::DataDir;
use qm_types::domain::{
    FindingAction, ReportOutcome, Severity, TaskKind, TaskStatus, VerificationLevel, WakeKind,
};
use qm_types::error::Refusal;
use qm_types::ids::{CommitSha, TaskId};
use rusqlite::Connection;

/// `events.kind` of a review starting.
pub const REVIEW_STARTED_EVENT: &str = "task.review_started";
/// `events.kind` of review findings.
pub const REVIEW_FINDINGS_EVENT: &str = "task.review_findings";
/// `events.kind` of a passed review.
pub const REVIEW_PASSED_EVENT: &str = "task.review_passed";
/// `events.kind` of a pass taken back because the builder reported a new head.
pub const REVIEW_STALE_EVENT: &str = "task.review_stale";

/// Whether `task`'s independent review passed on exactly `head`: a pass is recorded and its
/// `task.review_passed` event names that head. A commit after the pass needs a re-review.
pub fn passed_for(conn: &Connection, task: TaskId, head: &CommitSha) -> Result<bool, StoreError> {
    if store::facts(conn, task)?.is_none_or(|f| f.review_passed_at.is_none()) {
        return Ok(false);
    }
    Ok(task_store::last_event(conn, task, REVIEW_PASSED_EVENT)?
        .and_then(|(_, payload)| {
            payload
                .get("head")
                .and_then(serde_json::Value::as_str)
                .map(|h| h == head.as_str())
        })
        .unwrap_or(false))
}

/// The finding actions a re-review lists as already settled.
pub const SETTLED_ACTIONS: &[FindingAction] = &[FindingAction::AskUser, FindingAction::NoOp];

/// Whether `row` is gated by its review: Careful as it runs (direct PR), review not passed.
fn awaits_review(conn: &Connection, row: &TaskRow) -> Result<bool, StoreError> {
    if effective_of(row) != Some(VerificationLevel::Reviewed) {
        return Ok(false);
    }
    Ok(store::facts(conn, row.id)?.is_some_and(|f| f.review_passed_at.is_none()))
}

/// A report's findings.
fn findings(report: &ReportRow) -> Vec<Finding> {
    serde_json::from_value(report.review_findings.clone()).unwrap_or_default()
}

/// `file:line summary`.
fn finding_line(f: &Finding) -> String {
    match f.line {
        Some(line) => format!("{}:{line} {}", f.file, f.summary.trim()),
        None => format!("{} {}", f.file, f.summary.trim()),
    }
}

/// Every earlier `ask_user` or `no_op` finding on `builder`'s reviews, as `file:line summary`.
pub fn settled_findings(conn: &Connection, builder: TaskId) -> Result<Vec<String>, StoreError> {
    let mut out: Vec<String> = Vec::new();
    for report in store::review_reports(conn, builder)? {
        for f in findings(&report) {
            let line = finding_line(&f);
            if SETTLED_ACTIONS.contains(&f.action) && !out.contains(&line) {
                out.push(line);
            }
        }
    }
    Ok(out)
}

/// "3 things: 1 error, 2 warnings".
fn count_words(findings: &[Finding]) -> String {
    let n = findings.len();
    let by = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    let mut parts = Vec::new();
    for (sev, one, many) in [
        (Severity::Error, "error", "errors"),
        (Severity::Warning, "warning", "warnings"),
        (Severity::Info, "note", "notes"),
    ] {
        let k = by(sev);
        if k > 0 {
            parts.push(format!("{k} {}", if k == 1 { one } else { many }));
        }
    }
    let things = if n == 1 { "thing" } else { "things" };
    format!("{n} {things}: {}", parts.join(", "))
}

impl StartHook for VerificationService {
    /// A review task's start, inside `start_crewmate`'s transaction.
    fn on_recorded(
        &self,
        conn: &Connection,
        task: TaskId,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<Result<(), Refusal>, StoreError> {
        let _ = notify;
        let Some(review) = task_store::load(conn, task)? else {
            return Ok(Ok(()));
        };
        let (TaskKind::Review, Some(builder)) = (review.kind, review.builds_on) else {
            return Ok(Ok(()));
        };
        if let Some(other) = store::open_review_of(conn, builder, Some(task))? {
            return Ok(Err(Refusal::new(format!(
                "{other} is already reviewing {builder}"
            ))));
        }
        let Some(b) = task_store::load(conn, builder)? else {
            return Ok(Ok(()));
        };
        if !awaits_review(conn, &b)? {
            // Not Careful, or already passed: the review is only an answer.
            return Ok(Ok(()));
        }
        let done = reports::latest_clear(conn, builder)?
            .is_some_and(|r| r.outcome == Some(ReportOutcome::Done));
        match b.status {
            TaskStatus::Reported if done => {
                let to = match next_for(builder, b.status, TaskEvent::ReviewStarted) {
                    Ok(to) => to,
                    Err(r) => return Ok(Err(r)),
                };
                task_store::set_status(conn, builder, b.status, to, at)?;
            }
            // A re-review started by the supervisor: the builder is already in review.
            TaskStatus::InReview => {}
            _ => return Ok(Ok(())),
        }
        let head = review
            .base_commit
            .as_ref()
            .map_or_else(|| "its head".to_owned(), |h| h.as_str().to_owned());
        task_store::event(
            conn,
            REVIEW_STARTED_EVENT,
            builder,
            None,
            &format!("{task} started reviewing {builder} at {head}"),
            serde_json::json!({ "review": task, "head": head }),
            at,
        )?;
        Ok(Ok(()))
    }
}

impl VerificationService {
    /// A review task's clear done report, in the report's transaction.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn review_reported(
        &self,
        conn: &Connection,
        review: &TaskRow,
        report: &ReportRow,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<GateNote, StoreError> {
        let Some(builder) = review.builds_on else {
            return Ok(GateNote::default());
        };
        let Some(b) = task_store::load(conn, builder)? else {
            return Ok(GateNote::default());
        };
        if !awaits_review(conn, &b)? || b.status != TaskStatus::InReview {
            return Ok(GateNote::default());
        }
        let Some(facts) = store::facts(conn, builder)? else {
            return Ok(GateNote::default());
        };
        // Only the snapshot this reviewer actually started at can satisfy the gate. In
        // particular, a builder report arriving while the reviewer runs cannot relabel a pass.
        let Some(reviewed_head) = review.base_commit.as_ref() else {
            return Ok(GateNote {
                note_for_supervisor: Some(re_review_note(builder)),
            });
        };
        if review.project != b.project || b.head_commit.as_ref() != Some(reviewed_head) {
            return Ok(GateNote {
                note_for_supervisor: Some(format!(
                    "The review covered {reviewed_head}, not {builder}'s current reported head; {}",
                    re_review_note(builder)
                )),
            });
        }
        let found = findings(report);
        let severities: Vec<Severity> = found.iter().map(|f| f.severity).collect();
        let rounds = facts.review_rounds;
        let result = outcome(report.review_verdict, &severities, rounds);
        let head = b
            .head_commit
            .as_ref()
            .map_or_else(|| b.branch.clone(), |h| h.short().to_owned());
        let risk = report
            .review_risk
            .map_or_else(|| "unrated".to_owned(), |r| r.to_string());
        let task = review.id;
        let note = match result {
            ReviewOutcome::NotRecorded => {
                "The review has no verdict, so it records nothing.".to_owned()
            }
            ReviewOutcome::Pass => {
                store::set_review_rounds(conn, builder, rounds.saturating_add(1))?;
                store::set_review_passed(conn, builder, at)?;
                let to = next_for(builder, b.status, TaskEvent::ReviewPassed)
                    .map_err(|r| invalid(&r))?;
                task_store::set_status(conn, builder, b.status, to, at)?;
                task_store::event(
                    conn,
                    REVIEW_PASSED_EVENT,
                    builder,
                    None,
                    &format!("{builder}'s review passed · risk {risk}"),
                    serde_json::json!({
                        "review": task,
                        "report": report.id,
                        "risk": report.review_risk,
                        "head": reviewed_head,
                    }),
                    at,
                )?;
                wake_crewmate(
                    conn,
                    notify,
                    builder,
                    WakeKind::ReviewPassed,
                    format!("review_passed:{builder}:{}", report.id),
                    &format!(
                        "The independent review passed at {head} (risk {risk}). Push and open the \
                         pull request with pr_open, then keep going until every check passes."
                    ),
                    "review passed",
                    at,
                )?;
                format!("The review passed; {builder} now pushes and gets CI green.")
            }
            ReviewOutcome::Findings => {
                store::set_review_rounds(conn, builder, rounds.saturating_add(1))?;
                task_store::event(
                    conn,
                    REVIEW_FINDINGS_EVENT,
                    builder,
                    None,
                    &format!("{task} found {} in {builder}", count_words(&found)),
                    serde_json::json!({ "review": task, "report": report.id, "head": reviewed_head }),
                    at,
                )?;
                format!(
                    "Pass the auto_fix findings to {builder} word for word with message_task. \
                     Settle ask_user findings from the user's words, or ask the user (one question \
                     for the round). {builder} is re-reviewed after its fix."
                )
            }
            ReviewOutcome::HoldRoundLimit => {
                store::set_review_rounds(conn, builder, rounds.saturating_add(1))?;
                let first = found
                    .iter()
                    .find(|f| f.severity != Severity::Info)
                    .or(found.first())
                    .map_or_else(String::new, finding_line);
                let detail = format!(
                    "The review of {head} still has {} findings: {first}.",
                    found.len()
                );
                self.hold_builder(
                    conn,
                    builder,
                    HoldReason::ReviewRoundLimit,
                    &detail,
                    notify,
                    at,
                )?;
                format!(
                    "{builder} is held: its review still has findings after 2 re-reviews. The user \
                     has a decision card; add your view with present_hold."
                )
            }
            ReviewOutcome::HoldBlocked => {
                let reason = report
                    .review_risk_reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("no reason given");
                let detail = format!("The reviewer is blocked: {reason}.");
                self.hold_builder(conn, builder, HoldReason::NeedsUser, &detail, notify, at)?;
                format!(
                    "{builder} is held: its reviewer is blocked. The user has a decision card; add \
                     your view with present_hold."
                )
            }
        };
        Ok(GateNote {
            note_for_supervisor: Some(note),
        })
    }

    fn hold_builder(
        &self,
        conn: &Connection,
        builder: TaskId,
        reason: HoldReason,
        detail: &str,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<(), StoreError> {
        if let Some(facts) = HoldFacts::load(conn, builder, self.health(), at)?
            && let Err(r) = hold(conn, builder, reason, detail, &facts, notify, at)?
        {
            tracing::warn!(task = %builder, reason = %r, "the review couldn't hold the task");
        }
        Ok(())
    }

    /// A Careful builder's clear done report, in the
    /// report's transaction; the builder is already `reported`. Returns the note for the
    /// supervisor.
    pub(crate) fn builder_reported(
        conn: &Connection,
        row: &TaskRow,
        notify: &mut Notify,
        at: Timestamp,
    ) -> Result<Option<String>, StoreError> {
        let builder = row.id;
        // A pass counts only for the head it was given on: a done report at another head takes the
        // pass back, and the new head is re-reviewed.
        let reported = task_store::load(conn, builder)?.and_then(|r| r.head_commit);
        let passed = store::facts(conn, builder)?.is_some_and(|f| f.review_passed_at.is_some());
        if effective_of(row) == Some(VerificationLevel::Reviewed)
            && passed
            && let Some(head) = &reported
            && !passed_for(conn, builder, head)?
        {
            store::clear_review_passed(conn, builder)?;
            task_store::event(
                conn,
                REVIEW_STALE_EVENT,
                builder,
                None,
                &format!(
                    "{builder}'s review passed on an earlier head; {} needs a re-review",
                    head.short()
                ),
                serde_json::json!({ "head": head }),
                at,
            )?;
        }
        if !awaits_review(conn, row)? {
            return Ok(None);
        }
        let Some(facts) = store::facts(conn, builder)? else {
            return Ok(None);
        };
        if facts.review_rounds == 0 {
            return Ok(Some(format!(
                "{builder} is Careful: start its independent review with start_crewmate (kind \
                 review, builds_on {builder})."
            )));
        }
        // A re-review of the new head.
        let status = task_store::load(conn, builder)?.map_or(row.status, |r| r.status);
        if let Ok(to) = next_for(builder, status, TaskEvent::ReviewStarted) {
            task_store::set_status(conn, builder, status, to, at)?;
        }
        let Some(head) = reported else {
            return Ok(Some(re_review_note(builder)));
        };
        let Some((reviewer, reviewer_status, _)) = store::last_reviewer(conn, builder)? else {
            return Ok(Some(re_review_note(builder)));
        };
        if reviewer_status != TaskStatus::Reported {
            return Ok(Some(re_review_note(builder)));
        }
        let Ok(to) = next_for(reviewer, reviewer_status, TaskEvent::Steered) else {
            return Ok(Some(re_review_note(builder)));
        };
        // The head the last review started at, as its `task.review_started` event recorded it.
        let previous = task_store::last_event(conn, builder, REVIEW_STARTED_EVENT)?
            .and_then(|(_, p)| p.get("head").and_then(|h| h.as_str()).map(str::to_owned));
        if !pin_reviewer(conn, reviewer, &head)? {
            return Ok(Some(re_review_note(builder)));
        }
        task_store::set_status(conn, reviewer, reviewer_status, to, at)?;
        let settled = settled_findings(conn, builder)?;
        let mut text = format!(
            "Re-review {builder} at its new head {head}{}. Its branch {} is in this repository; \
             check every changed file again against the user's words.",
            previous
                .filter(|p| p != head.as_str())
                .map(|p| format!(" (you reviewed {p})"))
                .unwrap_or_default(),
            row.branch,
        );
        if !settled.is_empty() {
            text.push_str(
                " Already settled — don't report these again unless the new head changes them: ",
            );
            let listed = settled.join("; ");
            text.push_str(&listed);
            // A finding's own sentence usually ends the list already ("…is fine.").
            if !listed.ends_with(['.', '!', '?']) {
                text.push('.');
            }
        }
        wake_crewmate(
            conn,
            notify,
            reviewer,
            WakeKind::ReReview,
            format!("re_review:{reviewer}:{head}"),
            &text,
            &format!("re-review {builder}"),
            at,
        )?;
        task_store::event(
            conn,
            REVIEW_STARTED_EVENT,
            builder,
            None,
            &format!("{reviewer} started reviewing {builder} at {head}"),
            serde_json::json!({ "review": reviewer, "re_review": true, "head": head }),
            at,
        )?;
        Ok(Some(format!(
            "{builder} reported its fix; {reviewer} re-reviews it at {head}."
        )))
    }
}

/// Reusing a reviewer explicitly starts a new immutable source snapshot.
fn pin_reviewer(conn: &Connection, reviewer: TaskId, head: &CommitSha) -> Result<bool, StoreError> {
    let Some(row) = task_store::load(conn, reviewer)? else {
        return Ok(false);
    };
    let Some(base_ref) = row.base_ref.as_deref() else {
        return Ok(false);
    };
    task_store::set_base(conn, reviewer, row.base_branch.as_deref(), base_ref, head)?;
    Ok(true)
}

fn re_review_note(builder: TaskId) -> String {
    format!("Start {builder}'s re-review with start_crewmate (kind review, builds_on {builder}).")
}

fn invalid(r: &Refusal) -> StoreError {
    StoreError::Invalid {
        column: "tasks.status",
        message: r.reason.clone(),
    }
}

/// What a review brief adds about the builder: the evidence folder's manifest with its absolute
/// path, and, once the builder has been reviewed, the findings already settled.
pub async fn brief_extras(db: &Db, data_dir: &DataDir, builder: TaskId) -> Vec<String> {
    let folders = db
        .call(move |conn| qm_store::tasks::folder_label(conn, builder))
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| builder.own_label());
    let paths = data_dir.task(builder, folders);
    let listed = paths.clone();
    let files = tokio::task::spawn_blocking(move || manifest(&listed))
        .await
        .unwrap_or_default();
    let mut out = brief_lines(&paths, &files);
    match db.call(move |conn| settled_findings(conn, builder)).await {
        Ok(settled) if !settled.is_empty() => {
            out.push(
                "Already settled — don't report these again unless the new head changes them:"
                    .to_owned(),
            );
            out.extend(settled.into_iter().map(|s| format!("- {s}")));
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(task = %builder, error = %e, "couldn't read the settled findings");
        }
    }
    out
}

/// [`brief_extras`] appended to a review brief's builds-on body (the one line `briefs.rs` calls).
pub async fn append_brief_extras(db: &Db, data_dir: &DataDir, builder: TaskId, body: &mut String) {
    let extras = brief_extras(db, data_dir, builder).await;
    if extras.is_empty() {
        return;
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    body.push_str(&extras.join("\n"));
}
