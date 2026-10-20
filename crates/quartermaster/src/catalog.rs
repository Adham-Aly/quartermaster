//! The model catalog, handed to the core's routing (`qm_core::routing::ModelCatalog`).
//!
//! The catalog lives in `qm-providers`, which the core must not depend on (it never names a
//! concrete engine), so the binary adapts it here, as `crates/qm-core/src/routing.rs` sketches.

use qm_core::routing::{ModelCatalog, StarterTiers};
use qm_engine::DetectedModel;
use qm_providers::catalog::{Catalog as Embedded, detect, starter};
use qm_store::models::ModelFacts;
use qm_types::domain::Provider;
use qm_types::ids::ModelId;

/// The embedded catalog snapshot with its corrections.
#[derive(Debug)]
pub(crate) struct Catalog(pub &'static Embedded);

impl ModelCatalog for Catalog {
    fn resolve(&self, provider: Provider, detected: &DetectedModel) -> ModelFacts {
        let r = detect::resolve(self.0, provider, detected);
        ModelFacts {
            model: r.id,
            canonical: r.canonical,
            display_name: r.display_name,
            family: r.family,
            generation: r.generation,
            image_input: r.image_input,
            context_window: r.context_window,
            efforts: r.efforts,
            known: r.known,
        }
    }

    fn catalog_list(&self, provider: Provider) -> Vec<DetectedModel> {
        detect::catalog_list(self.0, provider)
    }

    fn starter_set(&self, provider: Provider) -> StarterTiers {
        let set = starter::starter_set(provider);
        StarterTiers {
            approved: set.approved,
            placements: set
                .placements
                .iter()
                .filter_map(|p| Some((p.tier, ModelId::new(p.canonical).ok()?)))
                .collect(),
        }
    }
}
