//! The Quartermaster's own model.
//!
//! "A separate setting that routing never changes. The default, Auto, tries high, then medium, then
//! premium, then low across your subscriptions. A change takes effect at the next idle moment,
//! never mid-turn. Within one engine the session just switches model. Moving between engines
//! (Claude to the provider engine or back) starts a new session that opens with a short summary of
//! the last few turns. Secondmates follow the same setting."

use crate::effort::{cap_to_supported, tier_default};
use crate::rank::{Candidate, Needs, placement_skip_reason, rank, skip_reason};
use qm_types::config::{QuartermasterModel, TierEfforts};
use qm_types::domain::{AccountKind, Effort, EngineKind, Tier};
use qm_types::ids::{AccountId, ModelId};

/// Auto's order: "high, then medium, then premium, then low".
pub const AUTO_ORDER: [Tier; 4] = [Tier::High, Tier::Medium, Tier::Premium, Tier::Low];

/// The orchestrators' model choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatChoice {
    pub model: ModelId,
    pub account: AccountId,
    pub tier: Option<Tier>,
    pub effort: Effort,
}

/// Picks the Quartermaster's (and secondmates') model from the setting. Auto looks only at
/// subscriptions; a named model may run on any account that serves it, subscriptions first.
/// `None` when nothing can serve.
///
/// Auto uses the selected tier entry's effort when no separate orchestrator effort is set, and
/// only accounts supporting that exact combo can serve it. A separate effort setting overrides
/// the combo and keeps its existing cap-to-supported semantics. A named orchestrator model also
/// keeps its separate semantics: explicit effort, else its tier default (high outside any tier),
/// capped by the model; it does not inherit a crew tier entry's effort.
pub fn choose(
    setting: &QuartermasterModel,
    effort: Option<Effort>,
    placed: &[Candidate],
    preferred: Option<AccountId>,
    tiers: &TierEfforts,
) -> Option<SeatChoice> {
    let needs = Needs::default();
    let usable = |c: &&Candidate| skip_reason(c, needs).is_none();
    let to_choice = |c: &Candidate, tier: Option<Tier>| SeatChoice {
        model: c.model.clone(),
        account: c.account,
        tier,
        effort: cap_to_supported(
            effort.unwrap_or_else(|| {
                if matches!(setting, QuartermasterModel::Auto)
                    && let Some(entry_effort) = c.effort_override
                {
                    return entry_effort;
                }
                tier_default(tiers, tier.unwrap_or(Tier::High))
            }),
            &c.efforts,
        ),
    };
    match setting {
        QuartermasterModel::Auto => AUTO_ORDER.iter().find_map(|&tier| {
            let mut ok: Vec<&Candidate> = placed
                .iter()
                .filter(|c| c.tier == tier && c.account_kind == AccountKind::Subscription)
                .filter(|c| {
                    if effort.is_none() {
                        placement_skip_reason(c, needs).is_none()
                    } else {
                        usable(c)
                    }
                })
                .collect();
            rank(&mut ok, preferred);
            ok.first().map(|c| to_choice(c, Some(tier)))
        }),
        QuartermasterModel::Model(model) => {
            let mut ok: Vec<&Candidate> = placed
                .iter()
                .filter(|c| &c.model == model)
                .filter(usable)
                .collect();
            rank(&mut ok, preferred);
            ok.first().map(|c| to_choice(c, Some(c.tier)))
        }
    }
}

/// How a change of the orchestrators' model is applied, always at the next idle moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchKind {
    /// Same model and account: nothing to do.
    Unchanged,
    /// "Within one engine the session just switches model" (`SessionCommand::SetModel`).
    SetModel,
    /// "Moving between engines … starts a new session that opens with a short summary of the last
    /// few turns."
    NewSessionWithSummary,
}

/// What a model change needs, given the engines before and after.
pub fn switch_kind(
    from: (&ModelId, EngineKind),
    to: (&ModelId, EngineKind),
    same_account: bool,
) -> SwitchKind {
    if from.1 != to.1 {
        SwitchKind::NewSessionWithSummary
    } else if from.0 == to.0 && same_account {
        SwitchKind::Unchanged
    } else {
        SwitchKind::SetModel
    }
}
