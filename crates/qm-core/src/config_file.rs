//! Loading and saving `config.toml`.
//!
//! A missing file means every default. Keys the schema doesn't know are reported in
//! [`Loaded::unknown_keys`] rather than refused or silently dropped, so a typo or a setting from a
//! newer version is visible. Saving is atomic: a temporary file in the same folder, flushed to
//! disk, then renamed over `config.toml`, with mode 0600.

use qm_types::config::Config;
use qm_types::data_dir::DataDir;
use qm_types::domain::{KindRow, Tier};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// A loaded config and the keys in the file that the schema doesn't know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub config: Config,
    /// Dotted paths such as `tiers.ultra`, in file order.
    pub unknown_keys: Vec<String>,
    /// Settings whose value this version can't use, each kept at its default: one plain line
    /// per setting, such as `routing.kinds.review = "fast" isn't a level; review follows its
    /// default (high)`.
    pub bad_values: Vec<String>,
}

/// Why `config.toml` couldn't be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("couldn't read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} isn't valid: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("couldn't serialise the settings: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("couldn't write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Loads `config.toml` from the data directory; a missing file gives every default.
pub fn load(dir: &DataDir) -> Result<Loaded, ConfigError> {
    let path = dir.config_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::default()),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    parse(&text).map_err(|source| ConfigError::Parse {
        path,
        source: Box::new(source),
    })
}

/// Parses the text of a `config.toml`, collecting unknown keys.
pub fn parse(text: &str) -> Result<Loaded, toml::de::Error> {
    let mut unknown_keys = Vec::new();
    let de = toml::Deserializer::parse(text)?;
    let config = serde_ignored::deserialize(de, |path| unknown_keys.push(path.to_string()))?;
    Ok(Loaded {
        config,
        unknown_keys,
        bad_values: bad_kind_tiers(text),
    })
}

/// The `[routing.kinds]` rows whose value isn't a tier. `KindTiers` loads each row on its own and
/// leaves these at their defaults; this names them for the log.
fn bad_kind_tiers(text: &str) -> Vec<String> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(kinds) = table
        .get("routing")
        .and_then(|r| r.get("kinds"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    kinds
        .iter()
        .filter_map(|(key, value)| {
            let row = key.parse::<KindRow>().ok()?;
            if value.as_str().is_some_and(|v| v.parse::<Tier>().is_ok()) {
                return None;
            }
            Some(format!(
                "routing.kinds.{key} = {value} isn't a level (premium, high, medium or low); \
                 {key} follows its default ({})",
                row.default_tier()
            ))
        })
        .collect()
}

/// Saves `config` as the data directory's `config.toml`, atomically.
pub fn save(dir: &DataDir, config: &Config) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(config)?;
    let path = dir.config_file();
    let tmp = dir
        .root()
        .join(format!(".config.toml.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    };
    write().map_err(|source| {
        let _ = fs::remove_file(&tmp);
        ConfigError::Write {
            path: path.clone(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::config::AutocompactWindow;

    #[test]
    fn a_partial_file_fills_in_the_defaults() {
        let loaded = parse("[compaction]\nautocompact_window = 300000\n").unwrap();
        assert_eq!(
            u32::from(loaded.config.compaction.autocompact_window),
            300_000
        );
        assert_eq!(loaded.config.notifications, Config::default().notifications);
        assert_eq!(loaded.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn an_out_of_range_window_loads_clamped_without_an_error() {
        let loaded = parse("[compaction]\nautocompact_window = 9000000\n").unwrap();
        assert_eq!(
            loaded.config.compaction.autocompact_window,
            AutocompactWindow::new(u32::MAX)
        );
        assert_eq!(loaded.unknown_keys, Vec::<String>::new());

        let loaded = parse("[compaction]\nautocompact_window = 120000\n").unwrap();
        assert_eq!(
            loaded.config.compaction.autocompact_window,
            AutocompactWindow::new(100_000)
        );
    }

    #[test]
    fn a_misspelt_window_key_is_reported_and_the_default_kept() {
        let loaded = parse("[compaction]\nautocompact_windw = 300000\n").unwrap();
        assert_eq!(
            loaded.config.compaction.autocompact_window,
            AutocompactWindow::default()
        );
        assert_eq!(loaded.unknown_keys, ["compaction.autocompact_windw"]);
    }

    #[test]
    fn a_window_that_is_not_a_number_is_an_error() {
        assert!(parse("[compaction]\nautocompact_window = \"big\"\n").is_err());
    }

    /// Every row, with `review` and `teach` changed.
    const ALL_ROWS: &str = "[routing.kinds]
lookup = \"low\"
build = \"high\"
build_on_finished_plan = \"medium\"
fix = \"high\"
fix_on_confirmed_diagnosis = \"medium\"
plan = \"high\"
research = \"high\"
teach = \"low\"
review = \"premium\"
reproduce = \"high\"
unclear = \"high\"
";

    #[test]
    fn no_kinds_section_means_every_default_and_none_is_written() {
        let loaded = parse("").unwrap();
        for &row in KindRow::ALL {
            assert_eq!(loaded.config.routing.kinds.tier(row), row.default_tier());
        }
        let text = toml::to_string_pretty(&loaded.config).unwrap();
        assert!(!text.contains("kinds"), "{text}");
    }

    #[test]
    fn the_first_change_writes_every_row() {
        let mut config = Config::default();
        config
            .routing
            .kinds
            .set(KindRow::Review, Some(Tier::Premium));
        config.routing.kinds.set(KindRow::Teach, Some(Tier::Low));
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains(ALL_ROWS), "{text}");
        // And it reads back the same.
        let back = parse(&text).unwrap();
        assert_eq!(
            back.config.routing.kinds.tier(KindRow::Review),
            Tier::Premium
        );
        assert_eq!(back.config.routing.kinds.tier(KindRow::Teach), Tier::Low);
        assert_eq!(
            toml::to_string_pretty(&back.config).unwrap(),
            text,
            "round trip"
        );
        assert_eq!(back.unknown_keys, Vec::<String>::new());
        assert_eq!(back.bad_values, Vec::<String>::new());
    }

    #[test]
    fn a_missing_row_follows_its_default_and_is_written_on_the_next_save() {
        let loaded = parse("[routing.kinds]\nreview = \"low\"\n").unwrap();
        let kinds = &loaded.config.routing.kinds;
        assert_eq!(kinds.tier(KindRow::Review), Tier::Low);
        assert_eq!(kinds.tier(KindRow::Build), Tier::High);
        assert_eq!(kinds.tier(KindRow::BuildOnFinishedPlan), Tier::Medium);
        let text = toml::to_string_pretty(&loaded.config).unwrap();
        assert!(
            text.contains("build_on_finished_plan = \"medium\""),
            "{text}"
        );
        assert!(text.contains("review = \"low\""), "{text}");
        let section = text.split("[routing.kinds]\n").nth(1).unwrap();
        let section = section.split("\n\n").next().unwrap();
        assert_eq!(section.lines().count(), KindRow::ALL.len(), "{text}");
    }

    #[test]
    fn an_unknown_kind_key_is_reported_once() {
        let loaded = parse("[routing.kinds]\nreview = \"low\"\nsurvey = \"high\"\n").unwrap();
        assert_eq!(loaded.unknown_keys, ["routing.kinds.survey"]);
        assert_eq!(loaded.config.routing.kinds.tier(KindRow::Review), Tier::Low);
    }

    #[test]
    fn a_bad_value_leaves_only_its_row_at_the_default() {
        let text = "[routing.kinds]\nreview = \"fast\"\nteach = 3\nplan = \"low\"\n\
                    [compaction]\nautocompact_window = 300000\n";
        let loaded = parse(text).unwrap();
        let kinds = &loaded.config.routing.kinds;
        assert_eq!(kinds.tier(KindRow::Review), Tier::High);
        assert_eq!(kinds.tier(KindRow::Teach), Tier::High);
        assert_eq!(kinds.tier(KindRow::Plan), Tier::Low);
        assert_eq!(
            u32::from(loaded.config.compaction.autocompact_window),
            300_000,
            "the rest of the file still loads"
        );
        assert_eq!(loaded.bad_values.len(), 2, "{:?}", loaded.bad_values);
        assert!(loaded.bad_values[0].contains("routing.kinds.review = \"fast\""));
        assert!(loaded.bad_values[0].contains("review follows its default (high)"));
        assert!(loaded.bad_values[1].contains("routing.kinds.teach = 3"));
        assert_eq!(loaded.unknown_keys, Vec::<String>::new());
    }

    #[test]
    fn resetting_every_row_removes_the_section() {
        let mut loaded = parse(ALL_ROWS).unwrap();
        loaded.config.routing.kinds = qm_types::config::KindTiers::default();
        let text = toml::to_string_pretty(&loaded.config).unwrap();
        assert!(!text.contains("kinds"), "{text}");
    }

    #[test]
    fn the_kinds_sit_beside_the_pin() {
        let text = "[routing]\nalways_use = { model = \"claude-opus-5-5\" }\n\n[routing.kinds]\nunclear = \"low\"\n";
        let loaded = parse(text).unwrap();
        assert!(loaded.config.routing.always_use.is_some());
        assert_eq!(
            loaded.config.routing.kinds.tier(KindRow::Unclear),
            Tier::Low
        );
        let back = parse(&toml::to_string_pretty(&loaded.config).unwrap()).unwrap();
        assert_eq!(back.config, loaded.config);
    }
}
