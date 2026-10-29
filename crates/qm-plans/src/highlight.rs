//! Code highlighting in Rust, as classes, so the page carries no highlighter script and both
//! themes are plain CSS: `syntect`'s classed HTML, with the light and dark theme rules scoped
//! under the page's `data-theme`.

use std::sync::OnceLock;
use syntect::highlighting::ThemeSet;
use syntect::html::{ClassStyle, ClassedHTMLGenerator, css_for_theme_with_class_style};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };
const LIGHT: &str = "InspiredGitHub";
const DARK: &str = "base16-ocean.dark";

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The highlighted lines of `code` in `lang`, or `None` for a language it doesn't know.
pub(crate) fn code(lang: &str, code: &str) -> Option<String> {
    let set = syntaxes();
    let syntax = set
        .find_syntax_by_token(lang)
        .filter(|s| s.name != "Plain Text")?;
    let mut generator = ClassedHTMLGenerator::new_with_class_style(syntax, set, STYLE);
    for line in LinesWithEndings::from(code) {
        generator
            .parse_html_for_line_which_includes_newline(line)
            .ok()?;
    }
    Some(generator.finalize())
}

/// The highlight rules for both themes.
pub(crate) fn css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        let rules = |name: &str| {
            themes
                .themes
                .get(name)
                .and_then(|theme| css_for_theme_with_class_style(theme, STYLE).ok())
                .unwrap_or_default()
        };
        format!(
            ":root[data-theme=light] {{\n{}}}\n:root[data-theme=dark] {{\n{}}}\n",
            rules(LIGHT),
            rules(DARK)
        )
    })
}
