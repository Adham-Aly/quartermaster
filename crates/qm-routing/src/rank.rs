//! How models in the same tier are ranked, and the capability check.
//!
//! "It skips models that can't handle the task, such as ones without image input when the task
//! has images, or with too small a context window, and records why each was skipped."
//!
//! The ranking of every (model, account) pair that passed:
//!
//! 1. "Your preferred account for crew work, if one is set and it can serve."
//! 2. "Subscriptions before pay-per-token keys. An API key has no included allowance to waste, so
//!    it serves only when no subscription can."
//! 3. "Allowance most at risk of going unused: lots left and a reset coming soon. Accounts with
//!    unknown usage rank below accounts known to be at risk, and above accounts known to be
//!    nearly used up. Unknown is never read as 'all left'."
//! 4. "The newest model in the same family."
//!
//! "When an account hits its limit, its running agents wait until the reset. … New tasks route
//! around that account until it resets": a cut-off account fails the capability check.

use qm_types::domain::{AccountKind, Effort, Tier};
use qm_types::ids::{AccountId, ModelId};
use std::cmp::Ordering;
use std::time::Duration;

/// One (model, account) pair placed in a tier: a row of `placements` joined with its model and
/// account.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub model: ModelId,
    /// The model's family (`claude-opus`, `gpt-6`), for "newest in the same family".
    pub family: String,
    /// The model's generation within its family; higher is newer.
    pub generation: u32,
    pub account: AccountId,
    pub account_kind: AccountKind,
    /// The tier this placement puts the model in.
    pub tier: Tier,
    /// Your own effort level for this model in this tier.
    pub effort_override: Option<Effort>,
    /// The levels the model supports (a set).
    pub efforts: Vec<Effort>,
    pub image_input: bool,
    /// Context window in tokens, if known.
    pub context_window: Option<u64>,
    pub usage: Usage,
    /// The account has hit its limit and hasn't reset yet.
    pub cut_off: bool,
}

/// What's known about an account's included allowance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Usage {
    /// No reading, or a missing or malformed one. Never read as "all left".
    Unknown,
    /// The tightest window's reading.
    Known {
        /// Fraction of the window used, 0.0 to 1.0.
        used: f64,
        /// Time until that window resets.
        resets_in: Duration,
    },
}

/// At or above this fraction used, an allowance counts as "nearly used up".
///
/// "Nearly used up" means 90%.
pub const NEARLY_USED_UP: f64 = 0.9;

/// Where a usage reading puts an account in step 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UsageClass {
    AtRisk,
    Unknown,
    NearlyUsedUp,
}

impl Usage {
    pub fn class(self) -> UsageClass {
        match self {
            Self::Unknown => UsageClass::Unknown,
            Self::Known { used, .. } if used >= NEARLY_USED_UP => UsageClass::NearlyUsedUp,
            Self::Known { .. } => UsageClass::AtRisk,
        }
    }

    /// "Lots left and a reset coming soon": the share left per hour until the reset. Higher is
    /// more at risk of going unused.
    ///
    /// How the two signals combine into one number.
    pub fn risk(self) -> f64 {
        match self {
            Self::Unknown => 0.0,
            Self::Known { used, resets_in } => {
                let left = (1.0 - used).clamp(0.0, 1.0);
                let hours = (resets_in.as_secs_f64() / 3600.0).max(1.0 / 60.0);
                left / hours
            }
        }
    }
}

/// What the task needs from a model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    pub image_input: bool,
    /// The context window the task needs, in tokens, if known.
    pub context_tokens: Option<u64>,
}

/// Why a pair can't serve the task, in plain words for the routing record; `None` if it can.
pub fn skip_reason(candidate: &Candidate, needs: Needs) -> Option<String> {
    if candidate.cut_off {
        return Some("its account is at its limit until it resets".to_owned());
    }
    if needs.image_input && !candidate.image_input {
        return Some("it has no image input and the task has images".to_owned());
    }
    if let (Some(need), Some(window)) = (needs.context_tokens, candidate.context_window)
        && window < need
    {
        return Some(format!(
            "its context window ({window} tokens) is smaller than the task needs ({need})"
        ));
    }
    None
}

/// Ranks pairs that passed the capability check, best first. Pairs that tie keep their input order
/// (placement order), so the result is deterministic.
pub fn rank(candidates: &mut [&Candidate], preferred: Option<AccountId>) {
    let newest: Vec<(String, u32)> = {
        let mut best: Vec<(String, u32)> = Vec::new();
        for c in candidates.iter() {
            match best.iter_mut().find(|(f, _)| *f == c.family) {
                Some((_, g)) => *g = (*g).max(c.generation),
                None => best.push((c.family.clone(), c.generation)),
            }
        }
        best
    };
    let is_newest = |c: &Candidate| {
        newest
            .iter()
            .any(|(f, g)| *f == c.family && *g == c.generation)
    };
    candidates.sort_by(|a, b| {
        let pref = |c: &Candidate| preferred != Some(c.account);
        pref(a)
            .cmp(&pref(b))
            .then_with(|| kind_rank(a.account_kind).cmp(&kind_rank(b.account_kind)))
            .then_with(|| a.usage.class().cmp(&b.usage.class()))
            .then_with(|| {
                b.usage
                    .risk()
                    .partial_cmp(&a.usage.risk())
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| is_newest(b).cmp(&is_newest(a)))
    });
}

const fn kind_rank(kind: AccountKind) -> u8 {
    match kind {
        AccountKind::Subscription => 0,
        AccountKind::ApiKey => 1,
    }
}
