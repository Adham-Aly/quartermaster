//! One rule for every override: it needs a turn you started.
//!
//! Every tool that takes an override calls [`check_override`] for each override present in its
//! arguments, before doing anything else. The Quartermaster's prompt decides whether your message
//! asked for it; this function decides whether the turn could carry it. Autonomy is stricter still:
//! no agent can change it at all, so it isn't an override.

use qm_types::domain::StartedBy;
use qm_types::error::Refusal;

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
}

impl OverrideKind {
    pub const ALL: &'static [Self] = &[
        Self::NamedModel,
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
    ];

    /// The override in a few words, as the refusal names it.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NamedModel => "a named model",
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
        }
    }
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
