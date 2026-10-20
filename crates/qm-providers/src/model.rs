//! What the engine needs to know about a model.
//!
//! [`ModelProfiles`] is the seam the model catalog fills later; until then [`BuiltinProfiles`]
//! lists the ChatGPT models Pi's catalog has for this login at the pinned commit, and an unknown
//! model gets cautious defaults: 128,000 tokens of context, no images, reasoning on.

use qm_engine::DetectedModel;
use qm_types::domain::{Effort, Provider};
use qm_types::ids::ModelId;
use std::fmt;

/// One model as the engine sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelProfile {
    pub id: ModelId,
    pub display_name: Option<String>,
    /// Tokens of context; compaction keeps below this minus a reserve.
    pub context_window: u32,
    /// The most output tokens one reply may have.
    pub max_output_tokens: u32,
    /// Whether it reasons (sends `reasoning` and replays encrypted reasoning).
    pub reasoning: bool,
    /// Whether it takes images; images to a model without them are downgraded to text.
    pub image_input: bool,
    /// The effort levels settings may offer.
    pub efforts: Vec<Effort>,
    /// Whether the profile came from a list (`false`: cautious defaults for an unknown model).
    pub known: bool,
}

impl ModelProfile {
    /// Cautious defaults for a model no list knows.
    pub fn unknown(id: ModelId) -> Self {
        Self {
            id,
            display_name: None,
            context_window: 128_000,
            max_output_tokens: 32_000,
            reasoning: true,
            image_input: false,
            efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            known: false,
        }
    }

    /// As the engine reports it from `Engine::models`.
    pub fn detected(&self) -> DetectedModel {
        DetectedModel {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            efforts: self.efforts.clone(),
            image_input: self.known.then_some(self.image_input),
            context_window: self.known.then_some(self.context_window),
        }
    }
}

/// Where model profiles come from: [`BuiltinProfiles`] or the catalog.
pub trait ModelProfiles: Send + Sync + fmt::Debug {
    /// The profile of `model` on `provider`; cautious defaults when it isn't known.
    fn profile(&self, provider: Provider, model: &ModelId) -> ModelProfile;

    /// The models this provider offers, for `Engine::models`.
    fn list(&self, provider: Provider) -> Vec<ModelProfile>;
}

/// The built-in list: the ChatGPT models of Pi's catalog at the pin.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinProfiles;

/// `(id, name, context window, images, efforts up to max)`.
const CHATGPT_MODELS: &[(&str, &str, u32, bool, bool)] = &[
    ("gpt-6-luna", "GPT-6 Luna", 272_000, true, true),
    ("gpt-6-sol", "GPT-6 Sol", 272_000, true, true),
    ("gpt-6-astra", "GPT-6 Astra", 272_000, true, true),
    ("gpt-6.1-sol", "GPT-6.1 Sol", 500_000, true, true),
    ("gpt-5.6-luna", "GPT-5.6 Luna", 272_000, true, true),
    ("gpt-5.6-terra", "GPT-5.6 Terra", 272_000, true, true),
    ("gpt-5.6-sol", "GPT-5.6 Sol", 272_000, true, true),
    ("gpt-5.5", "GPT-5.5", 272_000, true, false),
    (
        "gpt-5.3-codex-spark",
        "GPT-5.3 Codex Spark",
        128_000,
        false,
        false,
    ),
];

/// The cheapest ChatGPT model in [`BuiltinProfiles`] (live smoke runs use it).
pub const CHEAPEST_CHATGPT_MODEL: &str = "gpt-6-luna";

impl BuiltinProfiles {
    fn chatgpt() -> impl Iterator<Item = ModelProfile> {
        CHATGPT_MODELS
            .iter()
            .filter_map(|&(id, name, context, images, max)| {
                let id = ModelId::new(id).ok()?;
                let mut efforts =
                    vec![Effort::Low, Effort::Medium, Effort::High, Effort::ExtraHigh];
                if max {
                    efforts.push(Effort::Max);
                }
                Some(ModelProfile {
                    id,
                    display_name: Some(name.to_owned()),
                    context_window: context,
                    max_output_tokens: 128_000,
                    reasoning: true,
                    image_input: images,
                    efforts,
                    known: true,
                })
            })
    }
}

impl ModelProfiles for BuiltinProfiles {
    fn profile(&self, provider: Provider, model: &ModelId) -> ModelProfile {
        self.list(provider)
            .into_iter()
            .find(|p| &p.id == model)
            .unwrap_or_else(|| ModelProfile::unknown(model.clone()))
    }

    fn list(&self, provider: Provider) -> Vec<ModelProfile> {
        match provider {
            // An OpenAI API key reaches the same models through the same Responses API.
            Provider::ChatGpt | Provider::OpenAi => Self::chatgpt().collect(),
            _ => Vec::new(),
        }
    }
}

/// The output cap assumed when the catalog names none: generous, since the role's own cap
/// (8,192 for the orchestrators) is what the engine actually sends.
pub const CATALOG_MAX_OUTPUT_TOKENS: u32 = 32_000;

/// The context window assumed for a catalog model whose window the catalog doesn't know.
pub const CATALOG_CONTEXT_WINDOW: u32 = 128_000;

/// Model profiles from the built-in catalog: every provider's models with their context window,
/// image input and effort levels (a model with no effort levels doesn't reason, so no reasoning
/// setting is ever sent to it). A model the catalog doesn't list gets [`ModelProfile::unknown`]'s
/// cautious defaults.
///
/// This replaces [`BuiltinProfiles`] (ChatGPT only) in the app: with it, every other provider
/// had no models at all, so a connected Copilot, xAI, Kimi, OpenRouter or API-key account had
/// nothing to route to.
#[derive(Clone, Copy, Debug)]
pub struct CatalogProfiles(pub &'static crate::catalog::Catalog);

impl CatalogProfiles {
    fn of(m: &crate::catalog::CatalogModel) -> ModelProfile {
        ModelProfile {
            id: m.id.clone(),
            display_name: Some(m.name.clone()),
            context_window: m
                .context_window
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(CATALOG_CONTEXT_WINDOW),
            max_output_tokens: CATALOG_MAX_OUTPUT_TOKENS,
            reasoning: !m.efforts.is_empty(),
            image_input: m.image_input,
            efforts: m.efforts.clone(),
            known: true,
        }
    }
}

impl ModelProfiles for CatalogProfiles {
    fn profile(&self, provider: Provider, model: &ModelId) -> ModelProfile {
        self.0
            .lookup(provider, model)
            .or_else(|| {
                self.0
                    .by_canonical(provider, &crate::catalog::detect::canonical_id(model))
            })
            .map_or_else(|| ModelProfile::unknown(model.clone()), Self::of)
    }

    fn list(&self, provider: Provider) -> Vec<ModelProfile> {
        self.0.models(provider).iter().map(Self::of).collect()
    }
}

/// The Responses API's reasoning effort for our ladder (until the catalog maps it per model):
/// `low`/`medium`/`high`/`xhigh` pass through, `max` becomes `xhigh`.
pub const fn responses_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::ExtraHigh | Effort::Max => "xhigh",
    }
}
