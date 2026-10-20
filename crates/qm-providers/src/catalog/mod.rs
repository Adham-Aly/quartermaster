//! The model catalog.
//!
//! - **A snapshot built into the binary**, `catalog/snapshot.json`, generated from models.dev by
//!   `catalog/refresh.py`; the diff is reviewed like any code change. No catalog service at
//!   runtime: updates arrive when you rebuild.
//! - **A hand-kept corrections file**, `catalog/corrections.toml`, overlaid on the snapshot when it
//!   loads: effort levels and the provider value for each, compatibility flags, image support and
//!   context windows.
//! - [`detect`]: matching an account's own model list to the catalog. A model the catalog doesn't
//!   know gets cautious defaults (no image input, a modest context window) and is marked unknown,
//!   so its routing records can say so.
//! - [`starter`]: the starter tier sets, seeded the first time a provider is connected.
//!
//! [`Catalog::embedded`] parses and validates both files once; a malformed file is a build-time
//! mistake, reported as a [`CatalogError`] at startup.

pub mod detect;
pub mod starter;

use qm_types::domain::{Effort, Provider};
use qm_types::ids::ModelId;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The snapshot, as `refresh.py` writes it.
pub const SNAPSHOT_JSON: &str = include_str!("../../catalog/snapshot.json");
/// The corrections file.
pub const CORRECTIONS_TOML: &str = include_str!("../../catalog/corrections.toml");

/// A catalog file that doesn't parse or names something unknown.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("the catalog snapshot is invalid: {0}")]
    Snapshot(String),
    #[error("the catalog corrections are invalid: {0}")]
    Corrections(String),
}

/// One model as the catalog knows it, corrections applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModel {
    /// The id the provider uses, e.g. `claude-opus-5-5` or `anthropic/claude-opus-5-5`.
    pub id: ModelId,
    /// The id tier placements key on: the same model on two accounts shares one placement.
    pub canonical: ModelId,
    pub name: String,
    /// The family for "newest in the same family"; the id when models.dev names none.
    pub family: String,
    /// Higher is newer: the release date as `YYYYMMDD`, or 0 when unknown.
    pub generation: u32,
    pub image_input: bool,
    pub context_window: Option<u64>,
    /// The rungs of our ladder it supports (a set). Empty: no effort setting is sent.
    pub efforts: Vec<Effort>,
    /// The provider value each supported rung is sent as.
    pub effort_values: BTreeMap<Effort, String>,
    /// Compatibility flags for the adapters (Chat Completions, Responses, Anthropic Messages).
    pub compat: BTreeMap<String, bool>,
}

impl CatalogModel {
    /// The provider's value for `effort`, or `None` when this model takes no effort setting at
    /// that rung (an unsupported level is never sent).
    pub fn effort_value(&self, effort: Effort) -> Option<&str> {
        if !self.efforts.contains(&effort) {
            return None;
        }
        Some(
            self.effort_values
                .get(&effort)
                .map_or_else(|| effort.as_str(), String::as_str),
        )
    }
}

/// The whole catalog: every provider's models.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    providers: BTreeMap<Provider, Vec<CatalogModel>>,
}

#[derive(Deserialize)]
struct SnapshotFile {
    providers: BTreeMap<String, Vec<SnapshotModel>>,
}

#[derive(Deserialize)]
struct SnapshotModel {
    id: String,
    name: String,
    family: Option<String>,
    release_date: Option<String>,
    image_input: bool,
    context_window: Option<u64>,
    #[serde(default)]
    canonical: Option<String>,
    #[serde(default = "yes")]
    tool_call: bool,
}

const fn yes() -> bool {
    true
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorrectionsFile {
    #[serde(default)]
    providers: BTreeMap<String, Correction>,
    #[serde(default)]
    models: Vec<ModelCorrection>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Correction {
    efforts: Option<Vec<Effort>>,
    effort_values: Option<BTreeMap<Effort, String>>,
    compat: Option<BTreeMap<String, bool>>,
    image_input: Option<bool>,
    context_window: Option<u64>,
    family: Option<String>,
    canonical: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCorrection {
    provider: String,
    id: String,
    efforts: Option<Vec<Effort>>,
    effort_values: Option<BTreeMap<Effort, String>>,
    compat: Option<BTreeMap<String, bool>>,
    image_input: Option<bool>,
    context_window: Option<u64>,
    family: Option<String>,
    canonical: Option<String>,
}

impl ModelCorrection {
    fn correction(self) -> Correction {
        Correction {
            efforts: self.efforts,
            effort_values: self.effort_values,
            compat: self.compat,
            image_input: self.image_input,
            context_window: self.context_window,
            family: self.family,
            canonical: self.canonical,
        }
    }
}

fn parse_provider(name: &str, err: fn(String) -> CatalogError) -> Result<Provider, CatalogError> {
    name.parse()
        .map_err(|_| err(format!("unknown provider {name:?}")))
}

fn model_id(id: &str, err: fn(String) -> CatalogError) -> Result<ModelId, CatalogError> {
    ModelId::new(id).map_err(|e| err(e.to_string()))
}

/// `2026-07-15` → `20260715`; anything else → 0.
fn generation(release_date: Option<&str>) -> u32 {
    release_date
        .map(|d| d.chars().filter(char::is_ascii_digit).collect::<String>())
        .filter(|d| d.len() == 8)
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

impl Correction {
    fn apply(&self, m: &mut CatalogModel) -> Result<(), CatalogError> {
        if let Some(e) = &self.efforts {
            let mut e = e.clone();
            e.sort();
            e.dedup();
            m.efforts = e;
        }
        if let Some(v) = &self.effort_values {
            m.effort_values
                .extend(v.iter().map(|(k, v)| (*k, v.clone())));
        }
        if let Some(c) = &self.compat {
            m.compat.extend(c.iter().map(|(k, v)| (k.clone(), *v)));
        }
        if let Some(i) = self.image_input {
            m.image_input = i;
        }
        if let Some(c) = self.context_window {
            m.context_window = Some(c);
        }
        if let Some(f) = &self.family {
            m.family.clone_from(f);
        }
        if let Some(c) = &self.canonical {
            m.canonical = model_id(c, CatalogError::Corrections)?;
        }
        Ok(())
    }
}

impl Catalog {
    /// The catalog built into the binary, parsed once.
    pub fn embedded() -> Result<&'static Self, CatalogError> {
        static CATALOG: OnceLock<Result<Catalog, CatalogError>> = OnceLock::new();
        CATALOG
            .get_or_init(|| Self::parse(SNAPSHOT_JSON, CORRECTIONS_TOML))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Parses a snapshot and overlays the corrections on it.
    pub fn parse(snapshot_json: &str, corrections_toml: &str) -> Result<Self, CatalogError> {
        let snapshot: SnapshotFile = serde_json::from_str(snapshot_json)
            .map_err(|e| CatalogError::Snapshot(e.to_string()))?;
        let corrections: CorrectionsFile = toml::from_str(corrections_toml)
            .map_err(|e| CatalogError::Corrections(e.to_string()))?;

        let mut provider_corrections = BTreeMap::new();
        for (name, c) in corrections.providers {
            provider_corrections.insert(parse_provider(&name, CatalogError::Corrections)?, c);
        }

        let mut providers: BTreeMap<Provider, Vec<CatalogModel>> = BTreeMap::new();
        for (name, models) in snapshot.providers {
            let provider = parse_provider(&name, CatalogError::Snapshot)?;
            let defaults = provider_corrections.get(&provider);
            let mut out = Vec::with_capacity(models.len());
            for s in models.into_iter().filter(|s| s.tool_call) {
                let id = model_id(&s.id, CatalogError::Snapshot)?;
                let canonical = match &s.canonical {
                    Some(c) => model_id(c, CatalogError::Snapshot)?,
                    None => id.clone(),
                };
                let mut m = CatalogModel {
                    family: s.family.unwrap_or_else(|| canonical.as_str().to_owned()),
                    generation: generation(s.release_date.as_deref()),
                    id,
                    canonical,
                    name: s.name,
                    image_input: s.image_input,
                    context_window: s.context_window,
                    efforts: Vec::new(),
                    effort_values: BTreeMap::new(),
                    compat: BTreeMap::new(),
                };
                if let Some(d) = defaults {
                    d.apply(&mut m)?;
                }
                out.push(m);
            }
            providers.insert(provider, out);
        }

        for mc in corrections.models {
            let provider = parse_provider(&mc.provider, CatalogError::Corrections)?;
            let model = providers
                .get_mut(&provider)
                .and_then(|ms| ms.iter_mut().find(|m| m.id.as_str() == mc.id))
                .ok_or_else(|| {
                    CatalogError::Corrections(format!(
                        "{} has no model {:?} in the snapshot",
                        mc.provider, mc.id
                    ))
                })?;
            let (provider_name, id) = (mc.provider.clone(), mc.id.clone());
            mc.correction()
                .apply(model)
                .map_err(|e| CatalogError::Corrections(format!("{provider_name} {id}: {e}")))?;
        }
        Ok(Self { providers })
    }

    /// Every model the catalog lists for a provider, in snapshot order.
    pub fn models(&self, provider: Provider) -> &[CatalogModel] {
        self.providers.get(&provider).map_or(&[], Vec::as_slice)
    }

    /// A provider's model by the id the account uses.
    pub fn lookup(&self, provider: Provider, id: &ModelId) -> Option<&CatalogModel> {
        self.models(provider).iter().find(|m| &m.id == id)
    }

    /// A provider's model by its canonical id (for a placement, which names canonical ids).
    pub fn by_canonical(&self, provider: Provider, canonical: &ModelId) -> Option<&CatalogModel> {
        self.models(provider)
            .iter()
            .find(|m| &m.canonical == canonical)
    }
}
