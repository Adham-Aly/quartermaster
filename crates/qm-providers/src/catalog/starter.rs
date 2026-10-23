//! Starter tier sets.
//!
//! Legacy preset definitions, retained for explicit callers. Account detection no longer seeds
//! these sets: new installs start with empty tiers and saved placements are preserved.
//!
//! Placements name canonical model ids, so the same model reached through two accounts (Opus 5.5
//! through your `claude` and through OpenRouter) shares one placement. Every starter placement
//! uses its tier's default effort; the effort is capped by what the model supports when routed
//! ("Haiku 5.5: Medium, capped by what Haiku supports").
//!
//! **Only the Claude set is approved.** Every other set is a **draft pending your
//! approval**, drafted from the catalog snapshot: [`StarterSet::approved`] is `false` for them.
//! Premium is left empty in the drafts except where a vendor's premium model is in the list, since
//! premium is used only when asked for, and falls back to high when empty.

use qm_types::domain::{Provider, Tier};

/// One starter placement: a canonical model id in a tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StarterPlacement {
    pub tier: Tier,
    pub canonical: &'static str,
}

/// A provider's starter tier set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StarterSet {
    pub provider: Provider,
    /// `true` only for a set you approved; `false` for a draft.
    pub approved: bool,
    pub placements: &'static [StarterPlacement],
}

const fn p(tier: Tier, canonical: &'static str) -> StarterPlacement {
    StarterPlacement { tier, canonical }
}

/// The Claude set, approved.
const CLAUDE: &[StarterPlacement] = &[
    p(Tier::Premium, "claude-fable-5-1"),
    p(Tier::High, "claude-opus-5-5"),
    p(Tier::Medium, "claude-sonnet-5-5"),
    p(Tier::Low, "claude-haiku-5-5"),
];

// ---- DRAFTS pending your approval. Do not treat as decided. ----

/// DRAFT: OpenAI models through a ChatGPT subscription or an OpenAI key.
const OPENAI_DRAFT: &[StarterPlacement] = &[
    p(Tier::High, "gpt-6-sol"),
    p(Tier::Medium, "gpt-5.6-luna"),
    p(Tier::Low, "gpt-6-luna"),
];

/// DRAFT: Gemini.
const GEMINI_DRAFT: &[StarterPlacement] = &[
    p(Tier::High, "gemini-4-argon"),
    p(Tier::Medium, "gemini-3.8-flash"),
    p(Tier::Low, "gemini-3.5-flash-lite"),
];

/// DRAFT: Grok (xAI).
const XAI_DRAFT: &[StarterPlacement] = &[
    p(Tier::High, "grok-4.7"),
    p(Tier::Medium, "grok-4.3"),
    p(Tier::Low, "grok-4.3"),
];

/// DRAFT: Kimi.
const KIMI_DRAFT: &[StarterPlacement] = &[
    p(Tier::High, "k3"),
    p(Tier::Medium, "k3"),
    p(Tier::Low, "kimi-for-coding-highspeed"),
];

/// DRAFT: OpenRouter, using the Claude lineup it carries.
const OPENROUTER_DRAFT: &[StarterPlacement] = &[
    p(Tier::Premium, "claude-fable-5-1"),
    p(Tier::High, "claude-opus-5-5"),
    p(Tier::Medium, "claude-sonnet-5-5"),
    p(Tier::Low, "claude-haiku-5-5"),
];

/// DRAFT: GitHub Copilot.
const COPILOT_DRAFT: &[StarterPlacement] = &[
    p(Tier::High, "claude-opus-5-5"),
    p(Tier::High, "gpt-6-sol"),
    p(Tier::Medium, "claude-sonnet-5-5"),
    p(Tier::Medium, "gpt-5.6-luna"),
    p(Tier::Low, "claude-haiku-5-5"),
];

/// A provider's starter set. Custom endpoints and the scripted engine have none: "custom API
/// models stay unused until you place them".
pub fn starter_set(provider: Provider) -> StarterSet {
    let (approved, placements): (bool, &'static [StarterPlacement]) = match provider {
        Provider::Claude => (true, CLAUDE),
        // DRAFT: an Anthropic key on the provider engine gets the Claude lineup.
        Provider::Anthropic => (false, CLAUDE),
        Provider::ChatGpt | Provider::OpenAi => (false, OPENAI_DRAFT),
        Provider::Gemini => (false, GEMINI_DRAFT),
        Provider::Xai => (false, XAI_DRAFT),
        Provider::Kimi => (false, KIMI_DRAFT),
        Provider::OpenRouter => (false, OPENROUTER_DRAFT),
        Provider::Copilot => (false, COPILOT_DRAFT),
        Provider::Custom | Provider::Scripted => (false, &[]),
    };
    StarterSet {
        provider,
        approved,
        placements,
    }
}
