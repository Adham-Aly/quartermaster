//! The schema of `config.toml`: your settings, never secrets.
//!
//! Every section and field has a default, so a missing or empty file means every default and a
//! partial file fills in the rest. Loading and saving the file (atomically, reporting unknown keys
//! rather than dropping them) lives in `qm_core::config_file`; the binary loads it at startup and
//! the core saves it when you change a setting.
//!
//! What lives here and what lives in the database: `config.toml` holds your preferences that
//! aren't about a particular detected model or account (the Quartermaster's model and effort, the
//! "always use" pin, each tier's default effort, the folders `find_project_folder` searches,
//! notifications). Which models sit in which tier at which effort (a model may sit in one tier at
//! several efforts) are rows in `placements`; accounts, labels, the preferred account and pauses
//! are rows in `accounts`.

use crate::domain::{Effort, KindRow, Tier};
use crate::ids::ModelId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

/// The whole of `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub quartermaster: QuartermasterConfig,
    pub routing: RoutingConfig,
    pub compaction: CompactionConfig,
    pub tiers: TierEfforts,
    pub projects: ProjectsConfig,
    pub workspace: WorkspaceConfig,
    pub notifications: NotificationsConfig,
}

/// The personal workspace: a plain folder crewmates work in directly. Unset, it is
/// `<data dir>/workspace`. Set it only from Settings: the app checks the folder and refuses a change
/// while personal tasks are open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// An absolute path, or one starting with `~`.
    pub folder: Option<String>,
}

/// The Quartermaster's own model and effort: separate settings routing never changes.
/// Secondmates follow them too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuartermasterConfig {
    pub model: QuartermasterModel,
    /// `effort = "high"`: the level it runs at, capped by what its model supports. Absent, it
    /// follows the tier its model came from (High's default for a named model in no tier).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
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
    /// `[routing.kinds]`: the tier each task kind routes to. Absent until you change a row.
    #[serde(skip_serializing_if = "KindTiers::is_default")]
    pub kinds: KindTiers,
}

/// The tier each Settings ▸ Routing ▸ Task kinds row routes to: your value, or the row's default
/// ([`KindRow::default_tier`]).
///
/// Saved as every row at once (so the file shows the whole mapping), or not at all while you've
/// set none. Loading is per row: a row missing from the file follows its default, and a value
/// that isn't a tier is ignored for that row only (`qm_core::config_file` reports it).
#[derive(Clone, Debug, Default)]
pub struct KindTiers {
    set: BTreeMap<KindRow, Tier>,
}

impl PartialEq for KindTiers {
    /// Equal when every row routes the same and both are written to the file, or neither is.
    fn eq(&self, other: &Self) -> bool {
        self.is_default() == other.is_default()
            && KindRow::ALL.iter().all(|&r| self.tier(r) == other.tier(r))
    }
}

impl Eq for KindTiers {}

impl KindTiers {
    /// The tier `row` routes to.
    pub fn tier(&self, row: KindRow) -> Tier {
        self.set.get(&row).copied().unwrap_or(row.default_tier())
    }

    /// Whether `row` routes somewhere other than its default.
    pub fn changed(&self, row: KindRow) -> bool {
        self.tier(row) != row.default_tier()
    }

    /// Sets `row` to `tier`; `None` puts it back to its default.
    pub fn set(&mut self, row: KindRow, tier: Option<Tier>) {
        match tier {
            Some(t) => {
                self.set.insert(row, t);
            }
            None => {
                self.set.remove(&row);
            }
        }
    }

    /// Whether no row is set: the section is left out of `config.toml`.
    pub fn is_default(&self) -> bool {
        self.set.is_empty()
    }
}

impl Serialize for KindTiers {
    /// Every row, in [`KindRow::ALL`] order.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(KindRow::ALL.len()))?;
        for &row in KindRow::ALL {
            map.serialize_entry(row.as_str(), &self.tier(row))?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for KindTiers {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_map(KindTiersVisitor)
    }
}

struct KindTiersVisitor;

impl<'de> serde::de::Visitor<'de> for KindTiersVisitor {
    type Value = KindTiers;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a table of task kinds and tiers")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<KindTiers, A::Error> {
        let mut tiers = KindTiers::default();
        while let Some(key) = map.next_key::<String>()? {
            if let Ok(row) = key.parse::<KindRow>() {
                // A value that isn't a tier leaves this row at its default.
                if let AnyValue::Text(text) = map.next_value::<AnyValue>()?
                    && let Ok(tier) = text.parse::<Tier>()
                {
                    tiers.set.insert(row, tier);
                }
            } else {
                // Unknown: reported as "a setting this version doesn't know".
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(tiers)
    }
}

/// Any value, keeping only text: what a `[routing.kinds]` row reads before it is checked.
enum AnyValue {
    Text(String),
    Other,
}

impl<'de> Deserialize<'de> for AnyValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = AnyValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_str<E>(self, v: &str) -> Result<AnyValue, E> {
                Ok(AnyValue::Text(v.to_owned()))
            }
            fn visit_bool<E>(self, _: bool) -> Result<AnyValue, E> {
                Ok(AnyValue::Other)
            }
            fn visit_i64<E>(self, _: i64) -> Result<AnyValue, E> {
                Ok(AnyValue::Other)
            }
            fn visit_u64<E>(self, _: u64) -> Result<AnyValue, E> {
                Ok(AnyValue::Other)
            }
            fn visit_f64<E>(self, _: f64) -> Result<AnyValue, E> {
                Ok(AnyValue::Other)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<AnyValue, A::Error> {
                while a.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(AnyValue::Other)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<AnyValue, A::Error> {
                while a
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(AnyValue::Other)
            }
        }
        d.deserialize_any(V)
    }
}

/// The "always use" pin, with its fallback model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlwaysUse {
    pub model: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<ModelId>,
}

/// How agents compact their conversations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub autocompact_window: AutocompactWindow,
}

/// The autocompact window: the conversation size, in tokens, at which an agent compacts its
/// conversation a little before it, as Claude Code's `/autocompact` does. Always a whole number of
/// steps between [`Self::MIN`] and [`Self::MAX`]; a value outside that range loads clamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "i64", into = "u32")]
pub struct AutocompactWindow(u32);

impl AutocompactWindow {
    pub const MIN: u32 = 100_000;
    pub const MAX: u32 = 1_000_000;
    pub const STEP: u32 = 50_000;
    pub const DEFAULT: u32 = 200_000;

    /// Clamps `tokens` into the range and rounds it down to a step.
    #[must_use]
    pub fn new(tokens: u32) -> Self {
        let clamped = tokens.clamp(Self::MIN, Self::MAX);
        Self(Self::MIN + (clamped - Self::MIN) / Self::STEP * Self::STEP)
    }

    /// One step larger, stopping at the maximum.
    #[must_use]
    pub fn up(self) -> Self {
        Self::new(self.0.saturating_add(Self::STEP))
    }

    /// One step smaller, stopping at the minimum.
    #[must_use]
    pub fn down(self) -> Self {
        Self::new(self.0.saturating_sub(Self::STEP))
    }

    /// `"200k"` or `"1M"`, for the Settings row.
    #[must_use]
    pub fn label(self) -> String {
        if self.0.is_multiple_of(1_000_000) {
            format!("{}M", self.0 / 1_000_000)
        } else {
            format!("{}k", self.0 / 1_000)
        }
    }
}

impl Default for AutocompactWindow {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

impl From<i64> for AutocompactWindow {
    /// Reads a value from `config.toml`: out-of-range numbers are clamped, never refused.
    fn from(tokens: i64) -> Self {
        let tokens = tokens.clamp(i64::from(Self::MIN), i64::from(Self::MAX));
        Self::new(u32::try_from(tokens).unwrap_or(Self::MAX))
    }
}

impl From<AutocompactWindow> for u32 {
    fn from(window: AutocompactWindow) -> Self {
        window.0
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn window(tokens: u32) -> u32 {
        u32::from(AutocompactWindow::new(tokens))
    }

    #[test]
    fn new_clamps_to_the_range() {
        assert_eq!(window(0), 100_000);
        assert_eq!(window(99_999), 100_000);
        assert_eq!(window(5_000_000), 1_000_000);
        assert_eq!(window(u32::MAX), 1_000_000);
    }

    #[test]
    fn new_rounds_down_to_a_step() {
        assert_eq!(window(124_999), 100_000);
        assert_eq!(window(125_000), 100_000);
        assert_eq!(window(224_999), 200_000);
        assert_eq!(window(225_000), 200_000);
        assert_eq!(window(999_999), 950_000);
        assert_eq!(window(1_000_000), 1_000_000);
    }

    #[test]
    fn default_is_200k() {
        assert_eq!(
            AutocompactWindow::default(),
            AutocompactWindow::new(200_000)
        );
        assert_eq!(window(AutocompactWindow::DEFAULT), 200_000);
    }

    #[test]
    fn up_and_down_step_and_stop_at_the_ends() {
        let mid = AutocompactWindow::new(200_000);
        assert_eq!(u32::from(mid.up()), 250_000);
        assert_eq!(u32::from(mid.down()), 150_000);

        let min = AutocompactWindow::new(0);
        assert_eq!(u32::from(min.down()), 100_000);
        assert_eq!(u32::from(min.up()), 150_000);

        let max = AutocompactWindow::new(u32::MAX);
        assert_eq!(u32::from(max.up()), 1_000_000);
        assert_eq!(u32::from(max.down()), 950_000);
    }

    #[test]
    fn labels_say_k_or_m() {
        assert_eq!(AutocompactWindow::new(100_000).label(), "100k");
        assert_eq!(AutocompactWindow::new(150_000).label(), "150k");
        assert_eq!(AutocompactWindow::new(200_000).label(), "200k");
        assert_eq!(AutocompactWindow::new(950_000).label(), "950k");
        assert_eq!(AutocompactWindow::new(1_000_000).label(), "1M");
    }

    #[test]
    fn a_window_round_trips_through_toml() {
        let config = Config {
            compaction: CompactionConfig {
                autocompact_window: AutocompactWindow::new(300_000),
            },
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert!(
            text.contains("[compaction]\nautocompact_window = 300000"),
            "{text}"
        );
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn a_missing_compaction_section_loads_the_default() {
        let config: Config = toml::from_str("[routing]\n").unwrap();
        assert_eq!(
            config.compaction.autocompact_window,
            AutocompactWindow::default()
        );
        assert_eq!(u32::from(config.compaction.autocompact_window), 200_000);
    }

    #[test]
    fn an_out_of_range_value_loads_clamped() {
        let load = |value: &str| {
            let config: Config =
                toml::from_str(&format!("[compaction]\nautocompact_window = {value}\n")).unwrap();
            u32::from(config.compaction.autocompact_window)
        };
        assert_eq!(load("5"), 100_000);
        assert_eq!(load("-3"), 100_000);
        assert_eq!(load("123456"), 100_000);
        assert_eq!(load("9000000"), 1_000_000);
        assert_eq!(load("9223372036854775807"), 1_000_000);
    }
}
