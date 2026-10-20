//! The schema of `config.toml`: your settings, never secrets.
//!
//! Every section and field has a default, so a missing or empty file means every default and a
//! partial file fills in the rest. Loading and saving the file (atomically, reporting unknown keys
//! rather than dropping them) lives in `qm_core::config_file`; the binary loads it at startup and
//! the core saves it when you change a setting.
//!
//! What lives here and what lives in the database: `config.toml` holds your preferences that
//! aren't about a particular detected model or account (the Quartermaster's model, the "always
//! use" pin, each tier's default effort, the folders `find_project_folder` searches,
//! notifications). Which models sit in which tier, with per-placement effort overrides, are rows
//! in `placements`; accounts, labels, the preferred account and pauses are rows in `accounts`.

use crate::domain::Effort;
use crate::ids::ModelId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The whole of `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub quartermaster: QuartermasterConfig,
    pub routing: RoutingConfig,
    pub tiers: TierEfforts,
    pub projects: ProjectsConfig,
    pub notifications: NotificationsConfig,
}

/// The Quartermaster's own model: a separate setting routing never changes. Secondmates follow it
/// too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuartermasterConfig {
    pub model: QuartermasterModel,
}

/// `model = "auto"` or a model ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum QuartermasterModel {
    /// Tries high, then medium, then premium, then low across your subscriptions.
    #[default]
    Auto,
    /// Always this model.
    Model(ModelId),
}

impl QuartermasterModel {
    const AUTO: &'static str = "auto";
}

impl Serialize for QuartermasterModel {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str(Self::AUTO),
            Self::Model(m) => s.serialize_str(m.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for QuartermasterModel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == Self::AUTO {
            Ok(Self::Auto)
        } else {
            ModelId::new(s)
                .map(Self::Model)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Routing settings. Routing itself is always on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// "Always use one model for all crew work, ignoring tiers", off when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub always_use: Option<AlwaysUse>,
}

/// The "always use" pin, with its fallback model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlwaysUse {
    pub model: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<ModelId>,
}

/// Each tier's default effort. A placement's own effort override wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TierEfforts {
    pub premium: Effort,
    pub high: Effort,
    pub medium: Effort,
    pub low: Effort,
}

impl Default for TierEfforts {
    fn default() -> Self {
        Self {
            premium: Effort::ExtraHigh,
            high: Effort::ExtraHigh,
            medium: Effort::Medium,
            low: Effort::Medium,
        }
    }
}

/// Where `find_project_folder` looks: searched case-insensitively, up to three levels deep, for
/// folders containing a `.git`. Per Mac. `~` means your home folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectsConfig {
    pub search_roots: Vec<String>,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            search_roots: [
                "~/Developer",
                "~/code",
                "~/src",
                "~/projects",
                "~/repos",
                "~/work",
                "~/GitHub",
                "~/Documents/GitHub",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

/// Desktop notifications for questions, decisions and approvals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub enabled: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
