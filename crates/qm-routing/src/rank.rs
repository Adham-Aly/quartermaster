//! How models in the same tier are ranked, and the capability check.
//!
//! "It skips models that can't handle the task, such as ones without image input when the task
//! has images, or with too small a context window, and records why each was skipped."
//!
//! The ranking of every (model, account) pair that passed, for crew work ([`rank`]):
//!
//! 1. "Your preferred account for crew work, if one is set and it can serve."
//! 2. Quota left ([`Standing`]), read on the account's largest pool (its weekly window, a monthly
//!    allowance, a prepaid or key budget: whichever runs longest), in this order:
//!    1. accounts under their reserve mark (or with none) and under 90% used: least used first;
//!    2. subscriptions whose quota is unknown ("Unknown is never read as 'all left'");
//!    3. accounts under their mark but at 90% used or more: least used first;
//!    4. accounts at or past their reserve mark: the least far past it first, then the least used;
//!    5. pay-per-token keys whose quota is unknown: "An API key has no included allowance to
//!       waste, so it serves only when no subscription can."
//!
//!    With no reserve marks set, the known accounts are a plain comparison of quota used.
//!    Percentages are compared to a hundredth of a point, so 60 − 55 and 80 − 75 tie.
//! 3. Subscriptions before pay-per-token keys.
//! 4. "The newest model in the same family."
//!
//! The Quartermaster's own model ([`rank_steady`]) skips step 2: it re-picks at every idle moment,
//! and quota moving it between accounts would restart its conversation.
//!
//! "When an account hits its limit, its running agents wait until the reset. … New tasks route
//! around that account until it resets": a cut-off account fails the capability check.

use qm_types::domain::{AccountKind, Effort, Tier};
use qm_types::ids::{AccountId, ModelId};
use std::cmp::Ordering;

/// One (model, account) pair placed in a tier: a row of `placements` joined with its model and
/// account.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub provider: qm_types::domain::Provider,
    pub model: ModelId,
    /// The model's family (`claude-opus`, `gpt-6`), for "newest in the same family".
    pub family: String,
    /// The model's generation within its family; higher is newer.
    pub generation: u32,
    pub account: AccountId,
    pub account_kind: AccountKind,
    /// The tier this placement puts the model in.
    pub tier: Tier,
    /// The selected model+effort combo in this tier. `None` uses the tier's default rules.
    pub effort_override: Option<Effort>,
    /// The levels the model supports (a set).
    pub efforts: Vec<Effort>,
    pub image_input: bool,
    /// Context window in tokens, if known.
    pub context_window: Option<u64>,
    pub usage: Usage,
    /// Your reserve mark for its account, in percent used: at or past it, the account is held in
    /// reserve. `None`: no mark.
    pub reserve_at: Option<u8>,
    /// The account has hit its limit and hasn't reset yet.
    pub cut_off: bool,
}

/// What's known about an account's quota, read on its largest pool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Usage {
    /// No reading, or a missing or malformed one. Never read as "all left".
    Unknown,
    /// The largest pool's reading.
    Known {
        /// Percent of the pool used, 0 to 100.
        used_percent: f64,
    },
}

/// At or above this percent used, an allowance counts as "nearly used up".
pub const NEARLY_USED_UP: f64 = 90.0;

/// Where an account's quota puts it within a tier (module docs, step 2), best first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Standing {
    /// Under its reserve mark, or with none, and under [`NEARLY_USED_UP`].
    Open { used: f64 },
    /// A subscription with no reading.
    Unknown,
    /// Under its reserve mark, or with none, but at or over [`NEARLY_USED_UP`].
    NearlyUsedUp { used: f64 },
    /// At or past its reserve mark, by `past` points.
    Reserve { past: f64, used: f64 },
    /// A pay-per-token key with no reading.
    UnknownKey,
}

impl Standing {
    /// The standing of an account with this usage, kind and reserve mark.
    pub fn of(usage: Usage, kind: AccountKind, reserve_at: Option<u8>) -> Self {
        let Usage::Known { used_percent } = usage else {
            return match kind {
                AccountKind::Subscription => Self::Unknown,
                AccountKind::ApiKey => Self::UnknownKey,
            };
        };
        let used = hundredths(used_percent.clamp(0.0, 100.0));
        match reserve_at.map(|mark| hundredths(f64::from(mark))) {
            Some(mark) if used >= mark => Self::Reserve {
                past: used - mark,
                used,
            },
            _ if used >= hundredths(NEARLY_USED_UP) => Self::NearlyUsedUp { used },
            _ => Self::Open { used },
        }
    }

    /// "60% used", "60% used, past its 55% reserve mark", "quota unknown": for the routing record.
    pub fn describe(self, reserve_at: Option<u8>) -> String {
        match (self, reserve_at) {
            (Self::Reserve { used, .. }, Some(mark)) => {
                format!("{:.0}% used, past its {mark}% reserve mark", used / 100.0)
            }
            (Self::Open { used } | Self::NearlyUsedUp { used } | Self::Reserve { used, .. }, _) => {
                format!("{:.0}% used", used / 100.0)
            }
            (Self::Unknown, _) => "quota unknown".to_owned(),
            (Self::UnknownKey, _) => "pay-per-token, quota unknown".to_owned(),
        }
    }

    /// Whether the account is at or past its reserve mark.
    pub fn in_reserve(self) -> bool {
        matches!(self, Self::Reserve { .. })
    }

    const fn band(self) -> u8 {
        match self {
            Self::Open { .. } => 0,
            Self::Unknown => 1,
            Self::NearlyUsedUp { .. } => 2,
            Self::Reserve { .. } => 3,
            Self::UnknownKey => 4,
        }
    }

    /// Best first: by band, then within it as the module docs say.
    pub fn cmp_best_first(self, other: Self) -> Ordering {
        self.band()
            .cmp(&other.band())
            .then_with(|| match (self, other) {
                (Self::Open { used: a }, Self::Open { used: b })
                | (Self::NearlyUsedUp { used: a }, Self::NearlyUsedUp { used: b }) => {
                    a.total_cmp(&b)
                }
                (Self::Reserve { past: pa, used: ua }, Self::Reserve { past: pb, used: ub }) => {
                    pa.total_cmp(&pb).then_with(|| ua.total_cmp(&ub))
                }
                _ => Ordering::Equal,
            })
    }
}

/// A percentage in whole hundredths of a point, so float noise never breaks a tie
/// (`0.80 − 0.75` and `0.60 − 0.55` differ in the last bits).
fn hundredths(percent: f64) -> f64 {
    (percent * 100.0).round()
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

/// Checks a tier entry as a fixed model+effort combo, in addition to the task's capabilities.
/// An account must support that exact effort, not merely a lower rung. Empty effort sets cannot
/// serve an explicit combo either. Pins and explicit orchestrator settings do not use this check.
pub fn placement_skip_reason(candidate: &Candidate, needs: Needs) -> Option<String> {
    skip_reason(candidate, needs).or_else(|| {
        candidate.effort_override.and_then(|effort| {
            (!candidate.efforts.contains(&effort)).then(|| {
                format!("its account doesn't support the selected entry's {effort} effort")
            })
        })
    })
}

/// Ranks pairs that passed the capability check for crew work, best first (module docs). Pairs
/// that tie keep their input order (placement order), so the result is deterministic.
pub fn rank(candidates: &mut [&Candidate], preferred: Option<AccountId>) {
    rank_by(candidates, preferred, true);
}

/// Ranks pairs for the Quartermaster's own model: as [`rank`], without the quota step.
pub fn rank_steady(candidates: &mut [&Candidate], preferred: Option<AccountId>) {
    rank_by(candidates, preferred, false);
}

fn rank_by(candidates: &mut [&Candidate], preferred: Option<AccountId>, quota: bool) {
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
    let standing = |c: &Candidate| Standing::of(c.usage, c.account_kind, c.reserve_at);
    candidates.sort_by(|a, b| {
        let pref = |c: &Candidate| preferred != Some(c.account);
        pref(a)
            .cmp(&pref(b))
            .then_with(|| {
                if quota {
                    standing(a).cmp_best_first(standing(b))
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| kind_rank(a.account_kind).cmp(&kind_rank(b.account_kind)))
            .then_with(|| is_newest(b).cmp(&is_newest(a)))
    });
}

const fn kind_rank(kind: AccountKind) -> u8 {
    match kind {
        AccountKind::Subscription => 0,
        AccountKind::ApiKey => 1,
    }
}
