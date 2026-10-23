//! Verification levels: choosing one, and changing it on a running task.
//!
//! "How a task's verification level is chosen. The app works it out, never the model; the
//! Quartermaster only passes on your words. The first rule that applies wins:
//! 1. No level for work that builds in place: any task in a project whose delivery mode is build
//!    in place. Every other task has one, whatever its kind: the level applies when it delivers
//!    code, and an answer never reaches the code gates.
//! 2. Your words for this task, which can go lower or higher than the project's setting.
//! 3. The project's own verification setting, which only you change.
//! 4. `standard`.
//!
//! `quick` and `reviewed` are never chosen automatically. The model tier and escalation never
//! change the verification level."
//!
//! "Careful … Needs direct-PR delivery; on any other mode it ships as standard and says why."
//!
//! "Changing the level on a running task: raising it means the missing validation or review must
//! happen before the next push or merge, even if a pull request is already open. Lowering it
//! removes only the review hold. History, failed checks and other holds stay. A level change never
//! grants permission to merge."

use qm_types::domain::{DeliveryMode, VerificationLevel, VerificationSource};
use qm_types::error::Refusal;
use qm_types::ids::TaskId;

/// A task's level and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChosenLevel {
    pub level: VerificationLevel,
    pub source: VerificationSource,
}

/// Whether a task has a level at all (rule 1).
pub fn has_level(delivery: DeliveryMode) -> bool {
    delivery != DeliveryMode::BuildInPlace
}

/// The first rule that applies. `user` is a level from your words, already accepted by the override
/// check on your turn; `project` is the project's setting.
pub fn choose(
    delivery: DeliveryMode,
    user: Option<VerificationLevel>,
    project: Option<VerificationLevel>,
) -> Option<ChosenLevel> {
    if !has_level(delivery) {
        return None;
    }
    Some(match (user, project) {
        (Some(level), _) => ChosenLevel {
            level,
            source: VerificationSource::User,
        },
        (None, Some(level)) => ChosenLevel {
            level,
            source: VerificationSource::Project,
        },
        (None, None) => ChosenLevel {
            level: VerificationLevel::Standard,
            source: VerificationSource::Default,
        },
    })
}

/// The level the task actually runs under, with the reason when Careful ships as standard.
pub fn effective(
    level: VerificationLevel,
    delivery: DeliveryMode,
) -> (VerificationLevel, Option<&'static str>) {
    if level == VerificationLevel::Reviewed && delivery != DeliveryMode::DirectPr {
        (
            VerificationLevel::Standard,
            Some("Careful needs direct pull-request delivery, so this task ships as standard"),
        )
    } else {
        (level, None)
    }
}

/// What a level change does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelEffect {
    Unchanged,
    /// The missing validation (and review, when raised to Careful) must happen before the next
    /// push or merge, even with a pull request open.
    Raised {
        validation_required: bool,
        review_required: bool,
    },
    /// Only the review hold is removed; history, failed checks and other holds stay.
    Lowered {
        review_hold_removed: bool,
    },
}

/// A level change on a running task. Never grants permission to merge: the merge gate still
/// applies in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelChange {
    pub task: TaskId,
    pub from: VerificationLevel,
    pub to: VerificationLevel,
    pub effect: LevelEffect,
}

/// `set_verification` after its override check: refused for tasks with no level (build in
/// place).
pub fn change(
    task: TaskId,
    current: Option<VerificationLevel>,
    to: VerificationLevel,
    delivery: DeliveryMode,
    review_passed: bool,
) -> Result<LevelChange, Refusal> {
    let Some(from) = current else {
        return Err(Refusal::new(format!(
            "{task} has no verification level (build-in-place work)"
        )));
    };
    let eff_from = effective(from, delivery).0;
    let eff_to = effective(to, delivery).0;
    let effect = match eff_to.cmp(&eff_from) {
        std::cmp::Ordering::Equal => LevelEffect::Unchanged,
        std::cmp::Ordering::Greater => LevelEffect::Raised {
            validation_required: eff_from == VerificationLevel::Quick,
            review_required: eff_to == VerificationLevel::Reviewed && !review_passed,
        },
        std::cmp::Ordering::Less => LevelEffect::Lowered {
            review_hold_removed: eff_from == VerificationLevel::Reviewed && !review_passed,
        },
    };
    Ok(LevelChange {
        task,
        from,
        to,
        effect,
    })
}
