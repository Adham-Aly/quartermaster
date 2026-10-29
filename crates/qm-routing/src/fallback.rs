//! When a tier is empty, try the next stronger tier first.
//!
//! "Mine prefers the next stronger tier, except premium, and only then goes weaker. That matches
//! 'never weaker' for unclear work."
//!
//! | Work landed in | Tries, in order | Empty tier shows |
//! |---|---|---|
//! | low | low → medium → high | "low runs on medium" |
//! | medium | medium → high → low | "medium runs on high" |
//! | high | high → medium → low | "high runs on medium" |
//! | premium | premium → high → medium → low | "premium runs on high" |
//! | stronger-model retry to high or medium | its target tier only; otherwise waits | "waiting for a high model" |
//! | stronger-model retry to premium | as premium | "premium runs on high" |
//!
//! A fallback uses the selected tier entry's model+effort combo. Only legacy entries with no
//! explicit effort keep the landed tier's default, capped by the model actually used. Task and
//! later-step effort overrides still take precedence. The routing record shows each fallback
//! step. Premium is never a fallback target. Premium work, whether you asked for it on one task
//! or set a task kind to it, falls back like the rest: it waits only when no tier at all can
//! serve. A stronger-model retry to high or medium still waits for its target, since running
//! it weaker would defeat the retry.

use qm_types::domain::Tier;

/// The tiers to try, in order, for work that landed in `landed`.
pub const fn order(landed: Tier) -> &'static [Tier] {
    landed.fallback_order()
}

/// The tiers a stronger-model retry may use. A retry to high or medium uses its target only and
/// otherwise waits: "a retry with a stronger model never drops to a lower tier". A retry to premium
/// is a premium request, so it falls back like one ([`order`]).
pub const fn retry_order(target: Tier) -> &'static [Tier] {
    match target {
        Tier::Premium => order(Tier::Premium),
        Tier::High => &[Tier::High],
        Tier::Medium => &[Tier::Medium],
        Tier::Low => &[Tier::Low],
    }
}

/// The line an empty tier shows when work runs on another one: "low runs on medium".
pub fn runs_on_line(landed: Tier, used: Tier) -> String {
    format!("{landed} runs on {used}")
}

/// The line on a waiting stronger-model retry: "waiting for a high model".
pub fn retry_waiting_line(target: Tier) -> String {
    format!("waiting for a {target} model")
}
