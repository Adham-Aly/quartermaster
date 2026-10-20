//! CI polling and what "passed" means, `ci_results` and reruns.
//!
//! The poller watches only a task's pushed head, every [`POLL_EVERY`] while any check is pending
//! ([`POLL_SLOW`] when GitHub's rate limit runs low), and stops when the verdict is final: then
//! one transaction records it, tells verification (repair counting), wakes the crewmate once with
//! the result, and settles a branch-only delivery waiting on it.

use super::excerpts::{check_name, external, with_excerpts};
use super::{DeliveryService, Loaded, callers_task, verified_named};
use crate::inbox::Inbox;
use crate::settling::push::branch_checks_final;
use crate::tasks::lifecycle::is_exiting;
use crate::tasks::{failed, new_wake, refuse};
use qm_github::{CheckSnapshot, GithubError, PASSING_CONCLUSIONS, RunStatus};
use qm_policy::merge_gate::{Check, CheckState, ChecksOnCommit, ChecksVerdict, checks_verdict};
use qm_policy::verification::effective;
use qm_store::StoreError;
use qm_store::delivery::{self as delivery_store, ChecksState, verdict};
use qm_store::tasks as task_store;
use qm_store::time::to_millis;
use qm_tools::defs::{CheckResult, CheckStatus, CiReport};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{DeliveryMode, TaskStatus, VerificationLevel, WakeKind};
use qm_types::ids::{CommitSha, TaskId};
use std::fmt::Write as _;
use std::time::Duration;

/// How often a pending head is polled.
pub const POLL_EVERY: Duration = Duration::from_secs(30);
/// How often while GitHub's rate limit runs low, or GitHub can't be reached.
pub const POLL_SLOW: Duration = Duration::from_secs(120);
/// Remaining requests that count as low.
pub const RATE_LOW: u32 = 500;
/// How long after a push "no checks" is believed (Actions takes a few seconds to register).
pub const NO_CHECKS_GRACE: Duration = Duration::from_secs(120);
/// How long after the last pending check finished a missing required check is believed.
pub const REQUIRED_GRACE: Duration = Duration::from_secs(600);

/// `events.kind` of a passed verdict.
pub const CHECKS_PASSED_EVENT: &str = "checks.passed";
/// `events.kind` of a failed verdict (failures or a required check that never reported).
pub const CHECKS_FAILED_EVENT: &str = "checks.failed";
/// `events.kind` of "no checks ran".
pub const CHECKS_NONE_EVENT: &str = "checks.none";
/// `events.kind` of a rerun asked of GitHub.
pub const RERUN_EVENT: &str = "task.rerun";

/// A snapshot read the way the rules read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub verdict: ChecksVerdict,
    pub passed: u32,
    pub total: u32,
    /// Any run or status still pending.
    pub pending: bool,
}

fn run_state(status: RunStatus, conclusion: Option<&str>) -> CheckState {
    match status {
        RunStatus::Queued | RunStatus::InProgress => CheckState::Pending,
        // Success, neutral and skipped pass.
        RunStatus::Completed if conclusion.is_some_and(|c| PASSING_CONCLUSIONS.contains(&c)) => {
            CheckState::Succeeded
        }
        RunStatus::Completed => CheckState::Failed,
    }
}

fn status_state(state: &str) -> CheckState {
    match state {
        "success" => CheckState::Succeeded,
        "pending" => CheckState::Pending,
        _ => CheckState::Failed,
    }
}

/// "Passed" on one snapshot: every check run **and** commit status on that exact commit, and every
/// required check present.
pub fn read_snapshot(s: &CheckSnapshot) -> Reading {
    let runs: Vec<Check<'_>> = s
        .runs
        .iter()
        .map(|r| Check {
            name: &r.name,
            state: run_state(r.status, r.conclusion.as_deref()),
        })
        .collect();
    let statuses: Vec<Check<'_>> = s
        .statuses
        .iter()
        .map(|st| Check {
            name: &st.context,
            state: status_state(&st.state),
        })
        .collect();
    let required: Vec<&str> = s.required.names.iter().map(String::as_str).collect();
    let verdict = checks_verdict(&ChecksOnCommit {
        runs: &runs,
        statuses: &statuses,
        required: &required,
    });
    let all = || runs.iter().chain(statuses.iter());
    Reading {
        verdict,
        passed: u32::try_from(all().filter(|c| c.state == CheckState::Succeeded).count())
            .unwrap_or(u32::MAX),
        total: u32::try_from(all().count()).unwrap_or(u32::MAX),
        pending: all().any(|c| c.state == CheckState::Pending),
    }
}

fn verdict_word(v: &ChecksVerdict) -> &'static str {
    match v {
        ChecksVerdict::Passed => verdict::PASSED,
        ChecksVerdict::Pending => verdict::PENDING,
        ChecksVerdict::Failed(_) => verdict::FAILED,
        ChecksVerdict::MissingRequired(_) => verdict::MISSING_REQUIRED,
        ChecksVerdict::NoChecks => verdict::NO_CHECKS,
    }
}

/// A stored verdict as the rules' type.
pub fn stored_verdict(state: &ChecksState) -> ChecksVerdict {
    match state.verdict.as_str() {
        verdict::PASSED => ChecksVerdict::Passed,
        verdict::FAILED => ChecksVerdict::Failed(state.failing.clone()),
        verdict::MISSING_REQUIRED => ChecksVerdict::MissingRequired(state.failing.clone()),
        verdict::NO_CHECKS => ChecksVerdict::NoChecks,
        _ => ChecksVerdict::Pending,
    }
}

fn elapsed_ms(from_ms: i64, now_ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(now_ms.saturating_sub(from_ms)).unwrap_or(0))
}

fn scheduler_key(task: TaskId) -> String {
    format!("checks:{task}")
}

impl DeliveryService {
    /// Polls `head`'s checks after [`POLL_EVERY`] (or `delay`), replacing any poll of the task.
    pub fn arm(&self, task: TaskId, head: CommitSha) {
        self.arm_after(task, head, POLL_EVERY);
    }

    pub(crate) fn arm_after(&self, task: TaskId, head: CommitSha, delay: Duration) {
        let svc = self.clone();
        self.scheduler().after(
            scheduler_key(task),
            delay,
            Box::new(move || Box::pin(async move { svc.poll(task, head).await })),
        );
    }

    fn next_delay(&self) -> Duration {
        match self.github().rate().remaining {
            Some(left) if left < RATE_LOW => POLL_SLOW,
            _ => POLL_EVERY,
        }
    }

    /// One poll of `head`; a head that's no longer the task's pushed head stops.
    #[allow(clippy::too_many_lines)]
    async fn poll(&self, task: TaskId, head: CommitSha) {
        let loaded = match self.load(task).await {
            Ok(loaded) => loaded,
            Err(e) => {
                tracing::warn!(%task, error = ?e, "the checks poller couldn't read the task");
                self.arm_after(task, head, POLL_SLOW);
                return;
            }
        };
        if loaded.delivery.pushed_head.as_ref() != Some(&head)
            || loaded.row.status == TaskStatus::Closed
        {
            return;
        }
        let Some(state) = loaded
            .delivery
            .checks
            .clone()
            .filter(|c| c.head == head && !c.is_final)
        else {
            return;
        };
        let Some(repo) = loaded.repo() else {
            return;
        };
        let branch = state
            .required_branch
            .clone()
            .unwrap_or_else(|| loaded.target());
        let snapshot = match self.github().checks(&repo, &head, &branch).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                tracing::info!(%task, error = %e.plain(), "couldn't read the checks; polling again");
                let delay = match e {
                    GithubError::Unavailable(_) | GithubError::RateLimited { .. } => POLL_SLOW,
                    _ => self.next_delay(),
                };
                self.arm_after(task, head, delay);
                return;
            }
        };
        let reading = read_snapshot(&snapshot);
        let now = self.clock().now();
        let now_ms = to_millis(now);
        let quiet_since = if reading.pending {
            None
        } else {
            state.quiet_since.or(Some(now_ms))
        };
        let is_final = match &reading.verdict {
            ChecksVerdict::Passed | ChecksVerdict::Failed(_) => true,
            ChecksVerdict::Pending => false,
            ChecksVerdict::NoChecks => elapsed_ms(state.pushed_at, now_ms) >= NO_CHECKS_GRACE,
            ChecksVerdict::MissingRequired(_) => {
                quiet_since.is_some_and(|q| elapsed_ms(q, now_ms) >= REQUIRED_GRACE)
            }
        };
        let failing = match &reading.verdict {
            ChecksVerdict::Failed(names) | ChecksVerdict::MissingRequired(names) => names.clone(),
            _ => Vec::new(),
        };
        let mut next = ChecksState {
            is_final,
            verdict: verdict_word(&reading.verdict).to_owned(),
            failing: failing.clone(),
            passed: reading.passed,
            total: reading.total,
            at: now_ms,
            quiet_since,
            ..state.clone()
        };
        if !is_final {
            if !matches!(reading.verdict, ChecksVerdict::Pending) {
                // Waiting out a grace period: shown as still running.
                verdict::PENDING.clone_into(&mut next.verdict);
            }
            let write = next.clone();
            if let Err(e) = self
                .tasks()
                .write(move |tx, _| delivery_store::set_checks(tx, task, &write).map(drop))
                .await
            {
                tracing::warn!(%task, error = %e, "couldn't record the checks");
            }
            self.arm_after(task, head, self.next_delay());
            return;
        }

        // Final: the excerpts first (network), then one transaction.
        let mut excerpts: Vec<(String, String)> = Vec::new();
        if matches!(reading.verdict, ChecksVerdict::Failed(_)) {
            for name in &failing {
                if let Some(text) = self.excerpt(&repo, &snapshot, name).await {
                    excerpts.push((name.clone(), text));
                }
            }
        }
        let text = final_text(&loaded, &head, &reading, &failing, &excerpts);
        if let Err(e) = self.record_final(&loaded, head.clone(), next, text).await {
            tracing::warn!(%task, error = %e, "couldn't record the final checks; polling again");
            self.arm_after(task, head, POLL_EVERY);
        }
    }

    /// The excerpt for one failing check of a snapshot.
    async fn excerpt(
        &self,
        repo: &qm_github::RepoRef,
        snapshot: &CheckSnapshot,
        name: &str,
    ) -> Option<String> {
        if let Some(run) = snapshot.runs.iter().find(|r| r.name == name) {
            return match self.github().log_excerpt(repo, run).await {
                Ok(text) => text,
                Err(e) => Some(format!("(the log couldn't be read: {})", e.plain())),
            };
        }
        snapshot
            .statuses
            .iter()
            .find(|s| s.context == name)
            .and_then(qm_github::logs::status_excerpt)
    }

    async fn record_final(
        &self,
        loaded: &Loaded,
        head: CommitSha,
        state: ChecksState,
        text: FinalText,
    ) -> Result<(), StoreError> {
        let task = loaded.row.id;
        let at = self.clock().now();
        let verification = self.verification();
        let parent = self.parent();
        let crewmate = self
            .tasks()
            .db()
            .call(move |conn| qm_store::agents::crewmate_of(conn, task))
            .await?
            .map(|a| a.id);
        let branch_only = loaded.row.delivery == DeliveryMode::BranchOnly;
        self.tasks()
            .write(move |tx, notify| {
                if !delivery_store::set_checks(tx, task, &state)? {
                    // A newer push took over: this verdict is about an old head.
                    return Ok(());
                }
                task_store::event(
                    tx,
                    text.event_kind,
                    task,
                    None,
                    &text.summary,
                    serde_json::json!({
                        "head": head,
                        "verdict": state.verdict,
                        "failing": state.failing,
                        "passed": state.passed,
                        "total": state.total,
                    }),
                    at,
                )?;
                let follow =
                    verification.checks_final(tx, task, &head, &state.failing, notify, at)?;
                let Some(now) = task_store::load(tx, task)? else {
                    return Ok(());
                };
                if let Some(agent) = crewmate
                    && now.status != TaskStatus::Closed
                    && !is_exiting(now.status)
                {
                    let mut body = text.message.clone();
                    if let Some(note) = follow.note.as_deref() {
                        body = body.replacen("{note}", note, 1);
                    } else {
                        body = body.replacen(" {note}", "", 1).replacen("{note}", "", 1);
                    }
                    let mut key = format!("checks:{task}:{}:{}", head.short(), state.verdict);
                    if state.reruns > 0 {
                        let _ = write!(key, ":r{}", state.reruns);
                    }
                    Inbox::post(
                        tx,
                        &new_wake(
                            agent,
                            WakeKind::Checks,
                            task,
                            key,
                            &body,
                            &text.short,
                            serde_json::json!({ "head": head, "verdict": state.verdict }),
                            at,
                        ),
                        notify,
                    )?;
                }
                if branch_only && now.status == TaskStatus::Settling {
                    branch_checks_final(
                        tx,
                        notify,
                        &*parent,
                        task,
                        &head,
                        &stored_verdict(&state),
                        at,
                    )?;
                }
                Ok(())
            })
            .await
    }
}

/// The final verdict's event and the crewmate's one message (`{note}` is replaced by
/// verification's note, or dropped).
#[derive(Clone, Debug)]
struct FinalText {
    event_kind: &'static str,
    summary: String,
    message: String,
    short: String,
}

fn final_text(
    loaded: &Loaded,
    head: &CommitSha,
    reading: &Reading,
    failing: &[String],
    excerpts: &[(String, String)],
) -> FinalText {
    let task = loaded.row.id;
    let short = head.short();
    let (whose, paren) = match (loaded.row.delivery, loaded.delivery.pr_number) {
        (DeliveryMode::DirectPr, Some(pr)) => (format!("PR #{pr}"), format!(" (PR #{pr})")),
        _ => (task.to_string(), format!(" (branch {})", loaded.row.branch)),
    };
    let names: Vec<String> = failing.iter().map(|n| check_name(n)).collect();
    let (p, t) = (reading.passed, reading.total);
    match &reading.verdict {
        ChecksVerdict::Passed => FinalText {
            event_kind: CHECKS_PASSED_EVENT,
            summary: format!("{whose}: {p}/{t} checks passed on {short}"),
            message: format!("All green: {p} of {t} checks passed on {short}{paren}."),
            short: "all green".to_owned(),
        },
        ChecksVerdict::Failed(_) => {
            let level = loaded
                .row
                .verification
                .map(|(l, _)| effective(l, loaded.row.delivery).0);
            let declare = if matches!(
                level,
                Some(VerificationLevel::Standard | VerificationLevel::Reviewed)
            ) {
                " Declare a diagnosis for each with ci_diagnosis before you push a fix."
            } else {
                ""
            };
            let head_line = format!(
                "{} of {t} checks failed on {short}{paren}: {}.{declare} {{note}}",
                names.len(),
                names.join(", ")
            );
            FinalText {
                event_kind: CHECKS_FAILED_EVENT,
                summary: format!(
                    "{whose}: {} of {t} checks failed on {short}: {}",
                    names.len(),
                    names.join(", ")
                ),
                message: with_excerpts(&head_line, excerpts),
                short: "checks failed".to_owned(),
            }
        }
        ChecksVerdict::MissingRequired(_) => {
            let (noun, verb) = if names.len() == 1 {
                ("Required check", "never reported")
            } else {
                ("Required checks", "never reported")
            };
            FinalText {
                event_kind: CHECKS_FAILED_EVENT,
                summary: format!(
                    "{whose}: required checks never reported on {short}: {}",
                    names.join(", ")
                ),
                message: format!(
                    "{noun} {} {verb} on {short}, so the checks can't pass. Tell your supervisor (report blocked) if the workflow doesn't run it. {{note}}",
                    names.join(", ")
                ),
                short: "a required check never reported".to_owned(),
            }
        }
        ChecksVerdict::NoChecks | ChecksVerdict::Pending => FinalText {
            event_kind: CHECKS_NONE_EVENT,
            summary: format!("{task}: no CI checks ran on {short}"),
            message: format!(
                "No CI checks ran on {short}. Name the local checks you ran on this commit under Verified."
            ),
            short: "no checks ran".to_owned(),
        },
    }
}

/// `ci_results`: a fresh read of the checks on the caller's pushed head.
#[allow(clippy::too_many_lines)]
pub(crate) async fn ci_results(
    svc: &DeliveryService,
    caller: &Caller,
    check: Option<String>,
) -> Result<CiReport, ToolFailure> {
    let task = callers_task(caller)?;
    let loaded = svc.load(task).await?;
    let Some(head) = loaded.delivery.pushed_head.clone() else {
        return Err(refuse("Nothing is pushed yet; pr_open pushes your branch"));
    };
    let Some(repo) = loaded.repo() else {
        return Err(refuse(
            "the project's remote isn't on GitHub, so there are no check results to read",
        ));
    };
    let branch = loaded
        .delivery
        .checks
        .as_ref()
        .and_then(|c| c.required_branch.clone())
        .unwrap_or_else(|| loaded.target());
    let snapshot = svc
        .github()
        .checks(&repo, &head, &branch)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    let required = |name: &str| snapshot.required.names.iter().any(|r| r == name);
    let mut checks: Vec<CheckResult> = Vec::new();
    for run in &snapshot.runs {
        let status = match (run.status, run.conclusion.as_deref()) {
            (RunStatus::Queued | RunStatus::InProgress, _) => CheckStatus::Pending,
            (RunStatus::Completed, Some("success" | "neutral")) => CheckStatus::Passed,
            (RunStatus::Completed, Some("skipped")) => CheckStatus::Skipped,
            (RunStatus::Completed, Some("cancelled")) => CheckStatus::Cancelled,
            (RunStatus::Completed, _) => CheckStatus::Failed,
        };
        checks.push(CheckResult {
            name: check_name(&run.name),
            status,
            required: required(&run.name),
            log_excerpt: None,
        });
    }
    for st in &snapshot.statuses {
        checks.push(CheckResult {
            name: check_name(&st.context),
            status: match status_state(&st.state) {
                CheckState::Succeeded => CheckStatus::Passed,
                CheckState::Pending => CheckStatus::Pending,
                CheckState::Failed => CheckStatus::Failed,
            },
            required: required(&st.context),
            log_excerpt: None,
        });
    }
    for name in &snapshot.required.names {
        let present = snapshot.runs.iter().any(|r| &r.name == name)
            || snapshot.statuses.iter().any(|s| &s.context == name);
        if !present {
            checks.push(CheckResult {
                name: check_name(name),
                status: CheckStatus::Pending,
                required: true,
                log_excerpt: None,
            });
        }
    }
    // Excerpts: the named check's, or every failing one's, within the shared budget.
    let wanted: Vec<String> = match &check {
        Some(name) => vec![name.clone()],
        None => snapshot
            .runs
            .iter()
            .filter(|r| run_state(r.status, r.conclusion.as_deref()) == CheckState::Failed)
            .map(|r| r.name.clone())
            .chain(
                snapshot
                    .statuses
                    .iter()
                    .filter(|s| status_state(&s.state) == CheckState::Failed)
                    .map(|s| s.context.clone()),
            )
            .collect(),
    };
    if let Some(name) = &check
        && !snapshot.runs.iter().any(|r| &r.name == name)
        && !snapshot.statuses.iter().any(|s| &s.context == name)
    {
        return Err(refuse(format!(
            "there's no check named {} on {}",
            check_name(name),
            head.short()
        )));
    }
    let mut budget = super::excerpts::EXCERPTS_CHARS;
    for name in wanted {
        if budget == 0 {
            break;
        }
        let Some(text) = svc.excerpt(&repo, &snapshot, &name).await else {
            continue;
        };
        let text = external(&text, budget);
        budget = budget.saturating_sub(text.chars().count());
        let shown = check_name(&name);
        if let Some(result) = checks.iter_mut().find(|c| c.name == shown) {
            result.log_excerpt = Some(text);
        }
    }
    Ok(CiReport { head, checks })
}

/// Reruns one failing check on the pushed head and re-arms the poller (verification decides
/// whether it may).
pub(crate) async fn rerun(
    svc: &DeliveryService,
    task: TaskId,
    check: &str,
) -> Result<(), ToolFailure> {
    let loaded = svc.load(task).await?;
    let Some(head) = loaded.delivery.pushed_head.clone() else {
        return Err(refuse("Nothing is pushed yet; pr_open pushes your branch"));
    };
    let Some(repo) = loaded.repo() else {
        return Err(refuse("the project's remote isn't on GitHub"));
    };
    let branch = loaded
        .delivery
        .checks
        .as_ref()
        .and_then(|c| c.required_branch.clone())
        .unwrap_or_else(|| loaded.target());
    let snapshot = svc
        .github()
        .checks(&repo, &head, &branch)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    let Some(run) = snapshot.runs.iter().find(|r| r.name == check) else {
        if snapshot.statuses.iter().any(|s| s.context == check) {
            return Err(refuse(format!(
                "{} is a commit status from an outside service; it can't be rerun",
                check_name(check)
            )));
        }
        return Err(refuse(format!(
            "there's no check named {} on {}",
            check_name(check),
            head.short()
        )));
    };
    svc.github()
        .rerun(&repo, run)
        .await
        .map_err(|e| ToolFailure::Failed(e.plain()))?;
    let at = svc.clock().now();
    let name = check_name(check);
    let state_head = head.clone();
    svc.tasks()
        .write(move |tx, _| {
            let Some(d) = delivery_store::load(tx, task)? else {
                return Ok(());
            };
            if let Some(mut state) = d.checks.filter(|c| c.head == state_head) {
                state.reruns += 1;
                state.is_final = false;
                verdict::PENDING.clone_into(&mut state.verdict);
                state.quiet_since = None;
                state.pushed_at = to_millis(at);
                state.at = to_millis(at);
                delivery_store::set_checks(tx, task, &state)?;
            }
            task_store::event(
                tx,
                RERUN_EVENT,
                task,
                None,
                &format!("{task} reran {name} on {}", state_head.short()),
                serde_json::json!({ "check": name, "head": state_head }),
                at,
            )?;
            Ok(())
        })
        .await
        .map_err(|e| failed(&e))?;
    svc.arm(task, head);
    Ok(())
}

/// Startup: re-arms every pushed head whose checks aren't final.
pub(crate) async fn recover(svc: &DeliveryService) -> Result<(), StoreError> {
    let open = svc
        .tasks()
        .db()
        .call(|conn| delivery_store::open_checks(conn))
        .await?;
    for task in open {
        let head = svc
            .tasks()
            .db()
            .call(move |conn| delivery_store::load(conn, task))
            .await?
            .and_then(|d| d.pushed_head);
        if let Some(head) = head {
            svc.arm_after(task, head, Duration::from_secs(1));
        }
    }
    Ok(())
}

/// Whether the latest done report's Verified names local checks on `head` (the local-checks rule).
pub(crate) fn local_checks_named(
    conn: &rusqlite::Connection,
    task: TaskId,
    head: &CommitSha,
) -> Result<bool, StoreError> {
    Ok(qm_store::reports::latest_clear(conn, task)?
        .is_some_and(|r| verified_named(&r) && r.head_commit.as_deref() == Some(head.as_str())))
}
