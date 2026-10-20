//! Matching an account's model list to the catalog.
//!
//! "The account decides which models you can use. Each account's own model list is read at
//! startup and matched to the catalog for capabilities. A model the catalog doesn't know gets
//! cautious defaults (no image input, a modest context window) until the catalog learns it, and
//! its routing records say so."
//!
//! These are plain functions; reading the list (in the background, with the fallback to the last
//! list and then to the catalog) and recording it is the core's (`qm_core::routing`).
//!
//! What the account says wins over the catalog where the account says it: Claude's `claude`
//! reports each model's effort levels itself. Where the account is silent, the catalog decides;
//! where both are silent, the cautious defaults apply.

use super::{Catalog, CatalogModel};
use qm_engine::DetectedModel;
use qm_types::domain::{Effort, Provider};
use qm_types::ids::ModelId;

/// The context window assumed for a model nobody knows: modest, so a task that needs more is
/// routed elsewhere rather than overflowing.
///
/// "A modest context window": 32,000 tokens.
pub const CAUTIOUS_CONTEXT_WINDOW: u64 = 32_000;

/// The effort levels assumed for a model nobody knows: none, so no effort setting is ever sent
/// (an unsupported level is never sent).
pub const CAUTIOUS_EFFORTS: &[Effort] = &[];

/// A detected model with everything routing needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModel {
    /// The id the account uses.
    pub id: ModelId,
    /// The id placements key on.
    pub canonical: ModelId,
    pub display_name: Option<String>,
    pub family: String,
    pub generation: u32,
    pub image_input: bool,
    pub context_window: u64,
    pub efforts: Vec<Effort>,
    /// The catalog knows this model. `false` means the cautious defaults filled the gaps.
    pub known: bool,
}

/// The catalog entry for a detected id, trying the id itself and then its canonical form.
fn find<'c>(catalog: &'c Catalog, provider: Provider, id: &ModelId) -> Option<&'c CatalogModel> {
    catalog
        .lookup(provider, id)
        .or_else(|| catalog.by_canonical(provider, &canonical_id(id)))
}

/// Strips an aggregator's vendor prefix (`anthropic/claude-opus-5-5` → `claude-opus-5-5`), the
/// same prefixes `catalog/refresh.py` strips.
pub fn canonical_id(id: &ModelId) -> ModelId {
    const PREFIXES: [&str; 5] = ["anthropic/", "openai/", "google/", "x-ai/", "moonshotai/"];
    PREFIXES
        .iter()
        .find_map(|p| id.as_str().strip_prefix(p))
        .and_then(|rest| ModelId::new(rest).ok())
        .unwrap_or_else(|| id.clone())
}

/// Matches one detected model to the catalog.
pub fn resolve(catalog: &Catalog, provider: Provider, detected: &DetectedModel) -> ResolvedModel {
    let entry = find(catalog, provider, &detected.id);
    let mut detected_efforts = detected.efforts.clone();
    detected_efforts.sort();
    detected_efforts.dedup();
    match entry {
        Some(m) => ResolvedModel {
            id: detected.id.clone(),
            canonical: m.canonical.clone(),
            display_name: detected
                .display_name
                .clone()
                .or_else(|| Some(m.name.clone())),
            family: m.family.clone(),
            generation: m.generation,
            image_input: detected.image_input.unwrap_or(m.image_input),
            context_window: detected
                .context_window
                .map(u64::from)
                .or(m.context_window)
                .unwrap_or(CAUTIOUS_CONTEXT_WINDOW),
            efforts: if detected_efforts.is_empty() {
                m.efforts.clone()
            } else {
                detected_efforts
            },
            known: true,
        },
        None => ResolvedModel {
            id: detected.id.clone(),
            canonical: canonical_id(&detected.id),
            display_name: detected.display_name.clone(),
            family: canonical_id(&detected.id).as_str().to_owned(),
            generation: 0,
            // Cautious: no image input unless the account itself says so.
            image_input: detected.image_input.unwrap_or(false),
            context_window: detected
                .context_window
                .map_or(CAUTIOUS_CONTEXT_WINDOW, u64::from),
            efforts: if detected_efforts.is_empty() {
                CAUTIOUS_EFFORTS.to_vec()
            } else {
                detected_efforts
            },
            known: false,
        },
    }
}

/// Matches a whole list.
pub fn resolve_all(
    catalog: &Catalog,
    provider: Provider,
    detected: &[DetectedModel],
) -> Vec<ResolvedModel> {
    detected
        .iter()
        .map(|d| resolve(catalog, provider, d))
        .collect()
}

/// The catalog's list for a provider as if the account had reported it: the last resort when an
/// account's list can't be read and no earlier list was recorded.
pub fn catalog_list(catalog: &Catalog, provider: Provider) -> Vec<DetectedModel> {
    catalog
        .models(provider)
        .iter()
        .map(|m| DetectedModel {
            id: m.id.clone(),
            display_name: Some(m.name.clone()),
            efforts: m.efforts.clone(),
            image_input: Some(m.image_input),
            context_window: m.context_window.and_then(|c| u32::try_from(c).ok()),
        })
        .collect()
}
