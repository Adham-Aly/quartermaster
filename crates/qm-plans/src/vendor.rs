//! Vendored libraries, compiled in so a plan page works offline and asks nothing of the network:
//! `KaTeX` for math (MIT; `vendor/katex-0.16.47/LICENSE`), its fonts as `data:` URIs. Included in a
//! page only when the plan has math. `vendor/VERSIONS.toml` lists each file's sha256.

use base64::Engine as _;
use std::sync::OnceLock;

pub(crate) const KATEX_JS: &str = include_str!("../vendor/katex-0.16.47/katex.min.js");
const KATEX_CSS: &str = include_str!("../vendor/katex-0.16.47/katex.min.css");
pub(crate) const KATEX_LICENCE: &str = include_str!("../vendor/katex-0.16.47/LICENSE");

/// `KaTeX`'s woff2 fonts, by file name.
const KATEX_FONTS: &[(&str, &[u8])] = &[
    (
        "KaTeX_AMS-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_AMS-Regular.woff2"),
    ),
    (
        "KaTeX_Caligraphic-Bold.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Caligraphic-Bold.woff2"),
    ),
    (
        "KaTeX_Caligraphic-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Caligraphic-Regular.woff2"),
    ),
    (
        "KaTeX_Fraktur-Bold.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Fraktur-Bold.woff2"),
    ),
    (
        "KaTeX_Fraktur-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Fraktur-Regular.woff2"),
    ),
    (
        "KaTeX_Main-Bold.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Main-Bold.woff2"),
    ),
    (
        "KaTeX_Main-BoldItalic.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Main-BoldItalic.woff2"),
    ),
    (
        "KaTeX_Main-Italic.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Main-Italic.woff2"),
    ),
    (
        "KaTeX_Main-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Main-Regular.woff2"),
    ),
    (
        "KaTeX_Math-BoldItalic.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Math-BoldItalic.woff2"),
    ),
    (
        "KaTeX_Math-Italic.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Math-Italic.woff2"),
    ),
    (
        "KaTeX_SansSerif-Bold.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_SansSerif-Bold.woff2"),
    ),
    (
        "KaTeX_SansSerif-Italic.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_SansSerif-Italic.woff2"),
    ),
    (
        "KaTeX_SansSerif-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_SansSerif-Regular.woff2"),
    ),
    (
        "KaTeX_Script-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Script-Regular.woff2"),
    ),
    (
        "KaTeX_Size1-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Size1-Regular.woff2"),
    ),
    (
        "KaTeX_Size2-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Size2-Regular.woff2"),
    ),
    (
        "KaTeX_Size3-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Size3-Regular.woff2"),
    ),
    (
        "KaTeX_Size4-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Size4-Regular.woff2"),
    ),
    (
        "KaTeX_Typewriter-Regular.woff2",
        include_bytes!("../vendor/katex-0.16.47/fonts/KaTeX_Typewriter-Regular.woff2"),
    ),
];

/// `KaTeX`'s stylesheet with each font as a `data:` URI (woff2 only; every browser this page targets
/// reads it), so it loads nothing.
pub(crate) fn katex_css() -> &'static str {
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| {
        let mut css = KATEX_CSS.to_owned();
        for (name, bytes) in KATEX_FONTS {
            let stem = name.trim_end_matches(".woff2");
            let fallbacks = format!(
                ",url(fonts/{stem}.woff) format(\"woff\"),url(fonts/{stem}.ttf) format(\"truetype\")"
            );
            css = css.replace(&fallbacks, "");
            let data = format!(
                "url(data:font/woff2;base64,{})",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            css = css.replace(&format!("url(fonts/{name})"), &data);
        }
        css
    })
}
