//! The Bridge's column widths, remembered per Mac in `<data dir>/tui.toml`.
//!
//! Loading never fails: a missing or unreadable file, a bad value or an unknown key falls back to
//! the default for that value, and values are clamped to their ranges. Saving is atomic (a
//! temporary file, then a rename), and a failed save is the caller's to ignore: losing a
//! remembered width must never interrupt you.

use crate::app::{App, LEFT_DEFAULT, LEFT_RANGE, RIGHT_DEFAULT, RIGHT_RANGE};
use qm_types::data_dir::DataDir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The file's name in the data directory.
pub(crate) const FILE: &str = "tui.toml";

const HEADER: &str = "# The Bridge's column widths on this Mac, written by Quartermaster.\n";

/// The remembered layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Prefs {
    pub(crate) left_width: u16,
    pub(crate) right_width: u16,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            left_width: LEFT_DEFAULT,
            right_width: RIGHT_DEFAULT,
        }
    }
}

/// The file as written, each value read on its own so one bad value spoils only itself.
#[derive(Debug, Default, Deserialize)]
struct Raw {
    left_width: Option<toml::Value>,
    right_width: Option<toml::Value>,
}

fn width(value: Option<&toml::Value>, default: u16, range: (u16, u16)) -> u16 {
    value
        .and_then(toml::Value::as_integer)
        .map_or(default, |n| {
            let n = n.clamp(i64::from(range.0), i64::from(range.1));
            u16::try_from(n).unwrap_or(default)
        })
}

impl Prefs {
    /// Reads the file's text, falling back value by value.
    pub(crate) fn parse(text: &str) -> Self {
        let raw: Raw = toml::from_str(text).unwrap_or_default();
        Self {
            left_width: width(raw.left_width.as_ref(), LEFT_DEFAULT, LEFT_RANGE),
            right_width: width(raw.right_width.as_ref(), RIGHT_DEFAULT, RIGHT_RANGE),
        }
    }

    /// The file's text.
    pub(crate) fn render(self) -> String {
        let body = toml::to_string(&self).unwrap_or_default();
        format!("{HEADER}{body}")
    }

    /// The widths `app` shows now.
    pub(crate) fn of(app: &App) -> Self {
        Self {
            left_width: app.left_width,
            right_width: app.right_width,
        }
    }

    /// Puts the widths into `app`.
    pub(crate) fn apply(self, app: &mut App) {
        app.left_width = self.left_width;
        app.right_width = self.right_width;
        app.redraw();
    }
}

/// `<data dir>/tui.toml`, or `None` when the data directory can't be located.
pub(crate) fn default_path() -> Option<PathBuf> {
    DataDir::resolve().ok().map(|dir| dir.root().join(FILE))
}

/// Loads the file at `path`; the defaults if it can't be read.
pub(crate) fn load(path: &Path) -> Prefs {
    fs::read_to_string(path)
        .map(|text| Prefs::parse(&text))
        .unwrap_or_default()
}

/// Writes `prefs` to `path` atomically: `tui.toml.tmp`, then a rename.
pub(crate) fn save(path: &Path, prefs: Prefs) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, prefs.render())?;
    fs::rename(&tmp, path)
}

impl App {
    /// Remembers the current widths, if this run remembers anything; a failure is ignored.
    pub(crate) fn save_prefs(&self) {
        if let Some(path) = &self.prefs_path {
            let _ = save(path, Prefs::of(self));
        }
    }
}
