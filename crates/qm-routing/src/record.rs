//! The routing record, stored in `routing_decisions`.
//!
//! Every routing decision is recorded: the declared facts it read, the rule that matched, the tier
//! the work landed in and the tier, model, account and effort actually used, every model skipped
//! and why, every fallback step, and the one plain-language line the task's record shows ("You
//! asked for this model", "No high model was available", "Building from the plan in task T-0043").
//! A record with no choice is waiting: premium requests and stronger-model retries wait rather than
//! drop a tier.

use crate::decider::DecidedBy;
use qm_types::domain::{Effort, Provider, TaskKind, Tier};
use qm_types::ids::{AccountId, ModelId, TaskId};
use serde::{Deserialize, Serialize};

/// One routing decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRecord {
    /// `routing_decisions.facts`.
    pub facts: DeclaredFacts,
    /// The rule that matched, as written in the rule table.
    pub rule: String,
    pub decided_by: DecidedBy,
    pub tier_landed: Tier,
    /// `None` while waiting.
    pub choice: Option<RoutedChoice>,
    pub skipped: Vec<SkippedModel>,
    pub fallbacks: Vec<FallbackStep>,
    pub why: String,
}

impl RoutingRecord {
    pub fn tier_used(&self) -> Option<Tier> {
        self.choice.as_ref().map(|c| c.tier)
    }
}

/// The facts routing reads; never the task's text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredFacts {
    pub kind: TaskKind,
    pub builds_on: Option<BuildsOn>,
    /// The task has images, so models without image input are skipped.
    pub needs_image_input: bool,
    /// The lightest verification level, chosen by you: one effort step lower.
    pub quick_verification: bool,
    /// What you asked for in your own words, accepted only on your turns.
    pub user: UserChoices,
    /// The task whose `later_steps` entry was applied.
    pub later_step_from: Option<TaskId>,
    /// A review prefers a model of another provider than the builder's, within the landed tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_provider_other_than: Option<Provider>,
}

/// The exceptions you named: they come first, capped by what the model supports.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserChoices {
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// You asked for premium.
    pub premium: bool,
    /// A stronger-model retry you chose.
    pub stronger_retry: bool,
}

/// What the work builds on, checked as a fact by the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildsOn {
    pub task: TaskId,
    pub fact: BuildsOnFact,
}

/// What the app found the named task to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildsOnFact {
    /// A `plan` task that reported an answer.
    FinishedPlan,
    /// A reproduction that reported `reproduced: yes`, or a research report marked as a diagnosis.
    ConfirmedDiagnosis,
    /// Anything else: treated as building on nothing.
    Nothing,
}

/// The pair and effort routing chose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutedChoice {
    pub tier: Tier,
    pub model: ModelId,
    pub account: AccountId,
    pub effort: Effort,
}

/// A (model, account) pair that failed the capability check, and why, in plain words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedModel {
    pub model: ModelId,
    pub account: AccountId,
    pub reason: String,
}

/// One fallback step between tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackStep {
    pub from: Tier,
    pub to: Tier,
}
