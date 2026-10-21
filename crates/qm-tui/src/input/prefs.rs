//! The Bridge's column widths as older versions remembered them, in `<data dir>/tui.toml`.
//!
//! Only read now, once, when there is no `ui.json` yet (`persist`): the first start after the
//! update picks the widths up from here, and `ui.json` holds them from then on. Loading never
//! fails: a missing or unreadable file, a bad value or an unknown key falls back to the default
//! for that value, and values are clamped to their ranges.

use crate::app::{LEFT_DEFAULT, LEFT_RANGE, RIGHT_DEFAULT, RIGHT_RANGE};
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// The file's name in the data directory.
pub(crate) const FILE: &str = "tui.toml";

/// The remembered layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// Loads the file at `path`; the defaults if it can't be read.
pub(crate) fn load(path: &Path) -> Prefs {
    fs::read_to_string(path)
        .map(|text| Prefs::parse(&text))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_value_spoils_only_itself_and_widths_are_clamped() {
        let prefs = Prefs::parse("left_width = 500\nright_width = \"wide\"\n");
        assert_eq!(prefs.left_width, LEFT_RANGE.1);
        assert_eq!(prefs.right_width, RIGHT_DEFAULT);
        assert_eq!(Prefs::parse("not toml {"), Prefs::default());
    }
}
