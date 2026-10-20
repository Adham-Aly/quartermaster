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
//! | premium | premium only; otherwise waits | "premium requests wait" |
//! | stronger-model retry | its target tier only; otherwise waits | "waiting for a high model" |
//!
//! "A fallback keeps the effort level of the tier the work landed in, capped by the model actually
//! used, and the routing record shows the step." Premium is never a fallback target.

use qm_types::domain::Tier;

/// The tiers to try, in order, for work that landed in `landed`.
pub const fn order(landed: Tier) -> &'static [Tier] {
    match landed {
        Tier::Low => &[Tier::Low, Tier::Medium, Tier::High],
        Tier::Medium => &[Tier::Medium, Tier::High, Tier::Low],
        Tier::High => &[Tier::High, Tier::Medium, Tier::Low],
        Tier::Premium => &[Tier::Premium],
    }
}

/// The tiers a stronger-model retry may use: "its target tier only; otherwise waits". "A retry with
/// a stronger model never drops to a lower tier; it waits until that tier can serve".
pub fn retry_order(target: Tier) -> [Tier; 1] {
    [target]
}

/// The line an empty tier shows when work runs on another one: "low runs on medium".
pub fn runs_on_line(landed: Tier, used: Tier) -> String {
    format!("{landed} runs on {used}")
}

/// The line while premium work waits: "premium requests wait".
pub const PREMIUM_WAITS: &str = "premium requests wait";

/// The line on a waiting stronger-model retry: "waiting for a high model".
pub fn retry_waiting_line(target: Tier) -> String {
    format!("waiting for a {target} model")
}
