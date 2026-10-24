//! A task's verification level: the level it runs under, and changing it on a running task.
//!
//! Choosing a level at start is `qm_policy::verification::choose`; this module reads the
//! stored level through [`effective`] and carries out `set_verification`: raising it means the
//! missing validation or review must happen before the next push or merge (even with a pull request
//! open); lowering it removes only the review hold; any change supersedes pending merge and push
//! approvals and never grants permission to merge.

use super::{VerificationService, level_name, level_short, wake_crewmate};
use crate::held::HeldService;
use crate::inbox::Notify;
use crate::tasks::lifecycle::{TaskEvent, next_for};
use qm_policy::scope::check_direct_report;
use qm_policy::verification::{LevelEffect, change};
use qm_store::StoreError;
use qm_store::agents;
use qm_store::tasks::{self as task_store, TaskRow};
use qm_store::verification as store;
use qm_tools::defs::VerificationChanged;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::{DeliveryMode, Role, TaskStatus, VerificationLevel, WakeKind};
use qm_types::error::Refusal;
use qm_types::ids::TaskId;
use rusqlite::Connection;

/// `events.kind` of a level change.
pub const LEVEL_CHANGED_EVENT: &str = "task.level_changed";
/// Why a level change supersedes a pending approval.
pub const SETTLE_AGAIN: &str = "the verification level changed; settle again";
/// The held kinds a level change supersedes ("the verification level changed; settle again").
pub const SUPERSEDED_KINDS: &[&str] = &["merge", "push"];

/// The level a task runs under: Careful ships as standard off direct-PR delivery.
pub fn effective(level: VerificationLevel, delivery: DeliveryMode) -> VerificationLevel {
    qm_policy::verification::effective(level, delivery).0
}

/// The level `row` runs under, or `None` for tasks with no level.
pub fn effective_of(row: &TaskRow) -> Option<VerificationLevel> {
    row.verification
        .map(|(level, _)| effective(level, row.delivery))
}

/// The supervisor's name in a direct-report refusal: "the Quartermaster" or "S-02".
pub(crate) fn owner_label(
    conn: &Connection,
    supervisor: qm_types::ids::AgentId,
) -> Result<String, StoreError> {
    Ok(match agents::role_of(conn, supervisor)? {
        Some((Role::Secondmate, Some(id))) => id.to_string(),
        _ => "the Quartermaster".to_owned(),
    })
}

/// "Direct report only", inside a transaction.
pub(crate) fn check_direct(
    conn: &Connection,
    caller: &Caller,
    row: &TaskRow,
) -> Result<Result<(), Refusal>, StoreError> {
    if caller.agent == row.supervisor {
        return Ok(Ok(()));
    }
    let owner = owner_label(conn, row.supervisor)?;
    Ok(check_direct_report(
        caller.agent,
        row.id,
        row.supervisor,
        &owner,
    ))
}

/// The tool result's sentence and the crewmate's wake for one change.
struct Wording {
    effect: String,
    wake: Option<String>,
}

fn wording(
    task: TaskId,
    to: VerificationLevel,
    effect: LevelEffect,
    careful_off_pr: bool,
) -> Wording {
    let now_is = format!("Your verification level is now {}", level_name(to));
    match effect {
        LevelEffect::Unchanged if careful_off_pr => Wording {
            effect: format!("Careful needs a pull request, so {task} ships as standard."),
            wake: None,
        },
        LevelEffect::Unchanged => Wording {
            effect: format!("{task} is already {}.", level_short(to)),
            wake: None,
        },
        LevelEffect::Raised {
            validation_required,
            review_required,
        } => {
            let mut effect = Vec::new();
            let mut wake = vec![format!("{now_is}.")];
            if validation_required {
                effect.push(format!(
                    "The standard validation must be done and reported before {task}'s next push or merge."
                ));
                wake.push(
                    "Before your next push, do the standard validation (rebase, evidence, tests, \
                     read the diff, docs) and put it in the PR body or your report."
                        .to_owned(),
                );
            }
            if review_required {
                effect.push(format!(
                    "An independent review must pass before {task}'s next push or merge."
                ));
                wake.push(
                    "Commit locally and report done; don't push until the independent review passes."
                        .to_owned(),
                );
            }
            if effect.is_empty() {
                effect.push(format!(
                    "{task} is now {}; what that level needs has already been done.",
                    level_short(to)
                ));
            }
            Wording {
                effect: effect.join(" "),
                wake: Some(wake.join(" ")),
            }
        }
        LevelEffect::Lowered {
            review_hold_removed: true,
        } => Wording {
            effect: format!(
                "{task} no longer needs a review; its history, failed checks and other holds stay."
            ),
            wake: Some(format!(
                "{now_is}; the review isn't needed. Push with pr_open and get CI green."
            )),
        },
        LevelEffect::Lowered {
            review_hold_removed: false,
        } => Wording {
            effect: format!(
                "{task} is now {}; its history, failed checks and holds stay.",
                level_short(to)
            ),
            wake: Some(match to {
                VerificationLevel::Quick => format!("{now_is}: CI must be green, nothing more."),
                _ => format!(
                    "{now_is}: the standard validation, the five report headings and the \
                     failed-check rules apply."
                ),
            }),
        },
    }
}

impl VerificationService {
    /// `set_verification`, after the handler's override check.
    #[allow(clippy::too_many_lines)]
    pub async fn set_verification(
        &self,
        caller: &Caller,
        task: TaskId,
        level: VerificationLevel,
    ) -> Result<VerificationChanged, ToolFailure> {
        // Under delivery's lock, like `pr_open`: a pending settle card this change supersedes takes
        // its task out of `settling` in the same step.
        let locks = self.ci().locks();
        let _guard = match &locks {
            Some(locks) => Some(locks.lock(task).await),
            None => None,
        };
        let at = self.clock().now();
        let caller = *caller;
        let (outcome, notify) = self
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                let mut notify = Notify::new();
                let Some(row) = task_store::load(&tx, task)? else {
                    return Ok((Err(Refusal::new(format!("{task} doesn't exist"))), notify));
                };
                if let Err(r) = check_direct(&tx, &caller, &row)? {
                    return Ok((Err(r), notify));
                }
                // A merge GitHub hasn't confirmed yet could still land: no level change until
                // GitHub settles it.
                if let Some(head) = crate::settling::recover::merge_unconfirmed(&tx, task)? {
                    let why = crate::settling::recover::merge_waiting(&head);
                    return Ok((Err(Refusal::new(why)), notify));
                }
                if !crate::tasks::lifecycle::is_open(row.status)
                    || crate::tasks::lifecycle::is_exiting(row.status)
                {
                    return Ok((
                        Err(Refusal::new(format!(
                            "{task} is {}; its verification level can't change any more",
                            row.status
                        ))),
                        notify,
                    ));
                }
                let facts = store::facts(&tx, task)?.ok_or_else(|| StoreError::Invalid {
                    column: "tasks.id",
                    message: format!("{task} vanished"),
                })?;
                let current = row.verification.map(|(l, _)| l);
                let changed = match change(
                    task,
                    current,
                    level,
                    row.delivery,
                    facts.review_passed_at.is_some(),
                ) {
                    Ok(c) => c,
                    Err(r) => return Ok((Err(r), notify)),
                };
                let careful_off_pr =
                    level == VerificationLevel::Reviewed && row.delivery != DeliveryMode::DirectPr;
                let words = wording(task, level, changed.effect, careful_off_pr);
                let stored_changes = current != Some(level);
                if changed.effect == LevelEffect::Unchanged && !stored_changes {
                    tx.commit()?;
                    return Ok((
                        Ok(VerificationChanged {
                            task,
                            from: changed.from,
                            to: changed.to,
                            effect: words.effect,
                        }),
                        notify,
                    ));
                }

                store::set_level(&tx, task, level)?;
                match changed.effect {
                    LevelEffect::Raised {
                        validation_required: true,
                        ..
                    } => store::set_validation_pending(&tx, task, true)?,
                    LevelEffect::Lowered {
                        review_hold_removed: true,
                    } if matches!(row.status, TaskStatus::InReview | TaskStatus::Reported) => {
                        // Only the review hold goes: back to work, to push and get CI green.
                        if let Ok(to) = next_for(task, row.status, TaskEvent::Steered) {
                            task_store::set_status(&tx, task, row.status, to, at)?;
                        }
                    }
                    _ => {}
                }
                // A level change never grants permission to merge: pending approvals no longer
                // apply.
                for held in store::pending_held_of(&tx, task, SUPERSEDED_KINDS)? {
                    HeldService::supersede_with(
                        &tx,
                        held,
                        Some(task),
                        SETTLE_AGAIN,
                        at,
                    )?;
                }
                // A task waiting in `settling` for that approval goes back to `reported`, so
                // `settle_task` can run again (the lock means no merge request is in flight, and
                // one whose outcome GitHub hasn't confirmed was refused above).
                let mut words = words;
                if let Some(now) = task_store::load(&tx, task)?
                    && now.status == TaskStatus::Settling
                    && let Ok(to) = next_for(task, now.status, TaskEvent::SettleRefused)
                {
                    task_store::set_status(&tx, task, now.status, to, at)?;
                    task_store::event(
                        &tx,
                        crate::settling::SETTLE_REFUSED_EVENT,
                        task,
                        None,
                        &format!("{task} not settled: {SETTLE_AGAIN}"),
                        serde_json::json!({ "reason": SETTLE_AGAIN }),
                        at,
                    )?;
                    words.effect = format!(
                        "{} Its pending approval no longer applies; {task} is back to reported, so settle it again.",
                        words.effect
                    );
                }
                let event = task_store::event(
                    &tx,
                    LEVEL_CHANGED_EVENT,
                    task,
                    Some(caller.agent),
                    &format!("{task}'s verification changed to {}", level_name(level)),
                    serde_json::json!({
                        "from": changed.from,
                        "to": changed.to,
                        "effect": words.effect,
                    }),
                    at,
                )?;
                if let Some(text) = &words.wake {
                    wake_crewmate(
                        &tx,
                        &mut notify,
                        task,
                        WakeKind::LevelChanged,
                        format!("level_changed:{task}:{event}"),
                        text,
                        &format!("verification now {}", level_short(level)),
                        at,
                    )?;
                }
                tx.commit()?;
                Ok((
                    Ok(VerificationChanged {
                        task,
                        from: changed.from,
                        to: changed.to,
                        effect: words.effect,
                    }),
                    notify,
                ))
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        self.notify(&notify);
        Ok(outcome?)
    }
}
