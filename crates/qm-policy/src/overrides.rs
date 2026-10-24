//! Overrides require a user turn, or verified user-message provenance for model choices.
//!
//! Every tool checks its overrides before acting. Model, tier, effort and premium can cite an earlier
//! actual user message, verified by the host against app-recorded provenance; the prompt still
//! decides what those words ask for. Other overrides retain their current-user-turn rule.
//! Autonomy is stricter still: no agent can change it at all.

use qm_types::domain::StartedBy;
use qm_types::error::Refusal;
use qm_types::ids::MessageId;

/// What gives an override its authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// The turn the tool call belongs to.
    Turn(StartedBy),
    /// A choice you picked on a card, which the app carries out itself.
    CardChoice,
}

impl From<StartedBy> for Authority {
    fn from(started_by: StartedBy) -> Self {
        Self::Turn(started_by)
    }
}

/// Every typed override.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverrideKind {
    /// A named model (`start_crewmate`, `route_to_secondmate`).
    NamedModel,
    /// A user-ranked model level for one task.
    Tier,
    /// Premium for one task.
    Premium,
    /// An effort level.
    Effort,
    /// Models or effort for later steps of a workflow.
    LaterSteps,
    /// A stronger-model retry: your turn, or choice 2 on a decision card.
    StrongerModelRetry,
    /// A base branch on a task or a routed request.
    BaseBranch,
    /// A secondmate's standing base branch.
    StandingBranch,
    /// A task's verification level, at start (`start_crewmate`) or on a running task
    /// (`set_verification`).
    VerificationLevel,
    /// Saving a standing instruction.
    SaveInstruction,
    /// Dropping a standing instruction.
    DropInstruction,
    /// Approving a plan for the user (`approve_plan`).
    PlanApproval,
    /// The user's go-ahead on a review's findings (`approve_review`).
    ReviewGoAhead,
    /// Working in a git project's folder itself rather than a worktree (`start_crewmate`).
    InPlace,
}

impl OverrideKind {
    pub const ALL: &'static [Self] = &[
        Self::NamedModel,
        Self::Tier,
        Self::Premium,
        Self::Effort,
        Self::LaterSteps,
        Self::StrongerModelRetry,
        Self::BaseBranch,
        Self::StandingBranch,
        Self::VerificationLevel,
        Self::SaveInstruction,
        Self::DropInstruction,
        Self::PlanApproval,
        Self::InPlace,
        Self::ReviewGoAhead,
    ];

    /// The override in a few words, as the refusal names it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NamedModel => "a named model",
            Self::Tier => "a model tier",
            Self::Premium => "premium",
            Self::Effort => "an effort level",
            Self::LaterSteps => "models or effort for later steps",
            Self::StrongerModelRetry => "a stronger-model retry",
            Self::BaseBranch => "a base branch",
            Self::StandingBranch => "a secondmate's standing branch",
            Self::VerificationLevel => "a task's verification level",
            Self::SaveInstruction => "saving a standing instruction",
            Self::DropInstruction => "dropping a standing instruction",
            Self::PlanApproval => "approving a plan",
            Self::ReviewGoAhead => "a go-ahead on a review's findings",
            Self::InPlace => "working in place",
        }
    }
}

/// Model, tier, effort and premium may cite an earlier user message. The host MUST verify that the
/// source is a real user-authored message belonging to the caller (or a checked routed request),
/// not an agent's assertion. A citation does not authorize unrelated overrides.
pub fn check_choice_override(
    authority: Authority,
    source: Option<MessageId>,
    kind: OverrideKind,
) -> Result<(), Refusal> {
    if matches!(
        kind,
        OverrideKind::NamedModel
            | OverrideKind::Tier
            | OverrideKind::Effort
            | OverrideKind::Premium
    ) && source.is_some()
    {
        return Ok(());
    }
    check_override(authority, kind)
}

/// Accepts an override on a turn you started or as a card choice; refuses it with a plain reason
/// on any other turn.
pub fn check_override(authority: Authority, kind: OverrideKind) -> Result<(), Refusal> {
    match authority {
        Authority::Turn(StartedBy::User { .. }) | Authority::CardChoice => Ok(()),
        Authority::Turn(StartedBy::AppEvent) => Err(Refusal::new(format!(
            "{} needs a turn started by the user's own message",
            kind.describe()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_earlier_user_message_only_authorizes_model_tier_effort_and_premium() {
        let app = Authority::Turn(StartedBy::AppEvent);
        let source = Some(MessageId::new(7));
        for &kind in OverrideKind::ALL {
            let choice = matches!(
                kind,
                OverrideKind::NamedModel
                    | OverrideKind::Tier
                    | OverrideKind::Effort
                    | OverrideKind::Premium
            );
            assert_eq!(
                check_choice_override(app, source, kind).is_ok(),
                choice,
                "{kind:?}"
            );
            assert!(check_choice_override(app, None, kind).is_err(), "{kind:?}");
            assert!(check_choice_override(Authority::CardChoice, None, kind).is_ok());
            let user = Authority::Turn(StartedBy::User {
                message: MessageId::new(8),
            });
            assert!(check_choice_override(user, None, kind).is_ok());
        }
    }
}
