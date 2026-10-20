//! The effort level.
//!
//! "The effort level comes from the tier, then is adjusted":
//!
//! - "Your words come first: an effort level you name for the task wins, capped by what the model
//!   supports."
//! - "Defaults: premium and high run at Extra high; medium and low run at Medium." (The defaults
//!   are `config.toml`'s `[tiers]`, [`qm_types::config::TierEfforts`].)
//! - "Per-model override: you can set a different level for a model in a specific tier, and that
//!   wins."
//! - "Quick verification: a task at the lightest verification level runs one effort step lower,
//!   unless you chose the level explicitly. Here 'the level' means an effort level you set for
//!   that model, or named in your words, since quick verification is only ever your own choice."
//! - "Capped by the model: the level is always limited to what the model can actually support. An
//!   unsupported level is never sent."
//! - "Fixed for the conversation: a resumed conversation keeps the level it started with."
//!
//! A model's levels are a set, not a ceiling: "a cap moves along this ladder", so the cap picks the
//! highest supported level at or below the wanted one.

use qm_types::config::TierEfforts;
use qm_types::domain::{Effort, Tier};

/// A tier's default effort from settings.
pub fn tier_default(tiers: &TierEfforts, tier: Tier) -> Effort {
    match tier {
        Tier::Premium => tiers.premium,
        Tier::High => tiers.high,
        Tier::Medium => tiers.medium,
        Tier::Low => tiers.low,
    }
}

/// Everything the effort rule reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffortInput<'a> {
    /// The default of the tier the work landed in (a fallback keeps it).
    pub tier_default: Effort,
    /// Your own level for this model in that tier (a placement override), if set.
    pub placement_override: Option<Effort>,
    /// A level you named for this task in your words, or carried by a `later_steps` entry.
    pub named: Option<Effort>,
    /// The task runs at the lightest verification level (`quick`).
    pub quick_verification: bool,
    /// The levels the model supports. Empty means the model reports no levels at all.
    pub supported: &'a [Effort],
    /// The level a resumed conversation started with.
    pub resumed: Option<Effort>,
}

/// Where the level came from, for the routing record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortSource {
    /// A resumed conversation keeps its level.
    Resumed,
    /// Your words (or a later step you named).
    Named,
    /// Your level for this model in this tier.
    PlacementOverride,
    /// The tier's default.
    TierDefault,
}

/// The chosen level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffortChoice {
    pub effort: Effort,
    pub source: EffortSource,
    /// Quick verification took it one step lower.
    pub stepped_down: bool,
    /// The model's supported levels moved it.
    pub capped: bool,
}

/// The highest supported level at or below `wanted`; if the model supports none that low, its
/// lowest level (the nearest one it can run, since an unsupported level is never sent).
///
/// A model reporting no levels at all keeps `wanted` as the record of what routing asked for; the
/// engine sends no effort setting for such a model.
pub fn cap_to_supported(wanted: Effort, supported: &[Effort]) -> Effort {
    if supported.is_empty() {
        return wanted;
    }
    supported
        .iter()
        .copied()
        .filter(|e| *e <= wanted)
        .max()
        .or_else(|| supported.iter().copied().min())
        .unwrap_or(wanted)
}

/// The effort rule, first match wins.
pub fn choose_effort(input: EffortInput<'_>) -> EffortChoice {
    if let Some(effort) = input.resumed {
        return EffortChoice {
            effort,
            source: EffortSource::Resumed,
            stepped_down: false,
            capped: false,
        };
    }
    let (wanted, source) = if let Some(e) = input.named {
        (e, EffortSource::Named)
    } else if let Some(e) = input.placement_override {
        (e, EffortSource::PlacementOverride)
    } else {
        (input.tier_default, EffortSource::TierDefault)
    };
    let explicit = source != EffortSource::TierDefault;
    let stepped_down = input.quick_verification && !explicit && wanted != Effort::Low;
    let wanted = if stepped_down {
        wanted.step_down()
    } else {
        wanted
    };
    let effort = cap_to_supported(wanted, input.supported);
    EffortChoice {
        effort,
        source,
        stepped_down,
        capped: effort != wanted,
    }
}
