//! Held tasks and their fixed choices.
//!
//! "Every time the app holds a task for a decision, it attaches the same fixed choices; the
//! supervisor doesn't word them. That covers the CI repair limit, the review round limit, a stuck
//! task while one of those limits stands, and any blocked crewmate whose next move only you can
//! make."
//!
//! 1. "Retry with my guidance."
//! 2. "Try a stronger model. … It goes straight to the top tier when one is available to you,
//!    labelled 'Try a premium model'; otherwise one tier up, labelled 'Try a stronger model'. Only
//!    one of the two is ever shown, and it's left out once the task has already run at the top
//!    tier or has been escalated twice."
//! 3. "Discard the work."
//!
//! "Picking choice 2 counts as your explicit request … Escalation happens only through that
//! choice, or when you reject a result in your own words. A crewmate's claim of being stuck never
//! climbs a tier, and the app refuses any escalation that didn't come from your turn."
//!
//! "The supervisor contributes one short summary with `present_hold`; the app writes the title, the
//! reason for the hold and the choices, and works out which of 'premium' or 'stronger' to show. … A
//! stuck notice (Continue, Drop task, Discard) works the same way."

use crate::overrides::{Authority, OverrideKind, check_override};
use crate::repair::MAX_FAILED_REPAIRS;
use qm_types::api::HeldChoice;
use qm_types::domain::{TaskStatus, Tier};
use qm_types::error::Refusal;
use qm_types::ids::TaskId;
use qm_types::rules::HoldReason;

/// "Has been escalated twice": the limit on stronger-model retries per task chain.
pub const MAX_ESCALATIONS: u8 = 2;

/// The card's reason line for a hold, written by the app.
pub fn reason_line(reason: HoldReason) -> String {
    match reason {
        HoldReason::RepairLimit => {
            format!("Held by the app: {MAX_FAILED_REPAIRS} repair attempts used.")
        }
        HoldReason::ReviewRoundLimit => {
            "Held by the app: the review still has findings after 2 re-reviews.".to_owned()
        }
        HoldReason::Stuck => {
            "Held by the app: the crewmate stopped without reporting 80 minutes ago.".to_owned()
        }
        HoldReason::NeedsUser => "Held by the app: the next move is yours.".to_owned(),
    }
}

/// Choice 2's target: the top tier (premium) when one is available to you, otherwise the next
/// tier up that is. `None` when it's left out: the task already ran at the top tier, was escalated
/// twice, or nothing stronger is available.
///
/// When the tier one up has no usable model, this takes the next stronger tier that has one, never
/// a weaker one.
pub fn stronger_target(current: Tier, available: &[Tier], escalations: u8) -> Option<Tier> {
    if escalations >= MAX_ESCALATIONS || current == Tier::Premium {
        return None;
    }
    if available.contains(&Tier::Premium) {
        return Some(Tier::Premium);
    }
    Tier::ALL
        .iter()
        .copied()
        .filter(|t| *t > current && *t != Tier::Premium)
        .find(|t| available.contains(t))
}

/// Choice 2, labelled by the app ("Try a premium model" or "Try a stronger model").
pub fn stronger_choice(current: Tier, available: &[Tier], escalations: u8) -> Option<HeldChoice> {
    stronger_target(current, available, escalations).map(|tier| HeldChoice::StrongerModel {
        tier,
        premium: tier == Tier::Premium,
    })
}

/// A decision card's fixed choices, in order.
pub fn decision_choices(current: Tier, available: &[Tier], escalations: u8) -> Vec<HeldChoice> {
    let mut choices = vec![HeldChoice::RetryWithGuidance];
    choices.extend(stronger_choice(current, available, escalations));
    choices.push(HeldChoice::Discard);
    choices
}

/// The idle ladder's last step: a stuck notice (Continue, Drop task, Discard), or, "a stuck task
/// while one of those limits stands", the decision card's choices.
pub fn stuck_choices(
    limit_stands: bool,
    current: Tier,
    available: &[Tier],
    escalations: u8,
) -> Vec<HeldChoice> {
    if limit_stands {
        decision_choices(current, available, escalations)
    } else {
        vec![
            HeldChoice::Continue,
            HeldChoice::DropTask,
            HeldChoice::Discard,
        ]
    }
}

/// The card's title line: "Decision · C-0047 fix(ci): flaky upload test · api-server · high ·
/// opus-5-5".
pub fn card_title(task: TaskId, title: &str, project: &str, tier: Tier, model: &str) -> String {
    format!("Decision · {task} {title} · {project} · {tier} · {model}")
}

/// `present_hold`: "The task really is held."
pub fn check_present_hold(task: TaskId, status: TaskStatus) -> Result<(), Refusal> {
    if status == TaskStatus::Held {
        Ok(())
    } else {
        Err(Refusal::new(format!(
            "{task} isn't held (it's {status}); present_hold is only for held tasks"
        )))
    }
}

/// An answer is accepted only if the card stored that exact choice.
pub fn check_choice(stored: &[HeldChoice], chosen: &HeldChoice) -> Result<(), Refusal> {
    if stored.contains(chosen) {
        Ok(())
    } else {
        Err(Refusal::new(format!(
            "\"{}\" isn't one of this card's choices",
            chosen.label()
        )))
    }
}

/// A stronger-model retry (`start_crewmate.stronger_retry_of`, or choice 2): only from your turn
/// or a card choice, and at most twice per task chain. `escalations` is the retried task's count.
pub fn check_escalation(authority: Authority, escalations: u8) -> Result<(), Refusal> {
    check_override(authority, OverrideKind::StrongerModelRetry)?;
    if escalations >= MAX_ESCALATIONS {
        return Err(Refusal::new(
            "this task has already been escalated twice; a stronger-model retry isn't available",
        ));
    }
    Ok(())
}
