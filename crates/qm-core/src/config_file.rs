//! Loading and saving `config.toml`.
//!
//! A missing file means every default. Keys the schema doesn't know are reported in
//! [`Loaded::unknown_keys`] rather than refused or silently dropped, so a typo or a setting from a
//! newer version is visible. Saving is atomic: a temporary file in the same folder, flushed to
//! disk, then renamed over `config.toml`, with mode 0600.

use qm_types::config::Config;
use qm_types::data_dir::DataDir;
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
    })
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
        assert_eq!(loaded.config.routing, Config::default().routing);
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
}
