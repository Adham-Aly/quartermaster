//! The SVG allow-list cleaner. A plan's diagrams are SVG (a ```` ```svg ```` fence, or an
//! `assets/*.svg` image), drawn inline in the page so they follow its theme and text can be
//! selected. Inline SVG is markup in the page, so it is rebuilt from a parse, keeping only:
//!
//! - elements on [`ELEMENTS`] (shapes, text, gradients, markers, filters; never `script`,
//!   `foreignObject`, `style`, `image`, `a`, `use` of anything outside the drawing, animation);
//! - attributes on [`ATTRIBUTES`] (geometry and presentation; never an event handler);
//! - references only inside the drawing: `href` and `url(…)` must be `#id`;
//! - `style` attributes without `url(` to anything but `#id`, backslashes, `@` or `expression`.
//!
//! Everything else is dropped and named in [`Cleaned::dropped`], so the planner can see what didn't
//! survive. No DTD is accepted (entities can't expand). Every `id` gets the drawing's prefix, and
//! references follow, so two drawings on a page never share an id.

use std::collections::BTreeSet;
use std::fmt::Write as _;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
/// The largest drawing accepted.
pub const MAX_SVG_BYTES: usize = 2 * 1024 * 1024;

/// Elements kept.
pub const ELEMENTS: &[&str] = &[
    "svg",
    "g",
    "defs",
    "symbol",
    "use",
    "title",
    "desc",
    "path",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "text",
    "tspan",
    "textPath",
    "linearGradient",
    "radialGradient",
    "stop",
    "clipPath",
    "mask",
    "pattern",
    "marker",
    "filter",
    "feBlend",
    "feColorMatrix",
    "feComponentTransfer",
    "feComposite",
    "feDropShadow",
    "feFlood",
    "feFuncA",
    "feFuncB",
    "feFuncG",
    "feFuncR",
    "feGaussianBlur",
    "feMerge",
    "feMergeNode",
    "feMorphology",
    "feOffset",
];

/// Attributes kept (with `href`, checked separately).
pub const ATTRIBUTES: &[&str] = &[
    "id",
    "class",
    "style",
    "viewBox",
    "preserveAspectRatio",
    "width",
    "height",
    "x",
    "y",
    "x1",
    "y1",
    "x2",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "fx",
    "fy",
    "d",
    "points",
    "pathLength",
    "transform",
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-width",
    "stroke-opacity",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stroke-miterlimit",
    "opacity",
    "color",
    "visibility",
    "display",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "text-anchor",
    "dominant-baseline",
    "alignment-baseline",
    "baseline-shift",
    "letter-spacing",
    "word-spacing",
    "text-decoration",
    "dx",
    "dy",
    "rotate",
    "lengthAdjust",
    "textLength",
    "startOffset",
    "offset",
    "stop-color",
    "stop-opacity",
    "gradientUnits",
    "gradientTransform",
    "spreadMethod",
    "clip-path",
    "clip-rule",
    "clipPathUnits",
    "mask",
    "maskUnits",
    "maskContentUnits",
    "patternUnits",
    "patternContentUnits",
    "patternTransform",
    "marker-start",
    "marker-mid",
    "marker-end",
    "markerWidth",
    "markerHeight",
    "markerUnits",
    "refX",
    "refY",
    "orient",
    "filter",
    "filterUnits",
    "primitiveUnits",
    "in",
    "in2",
    "result",
    "mode",
    "operator",
    "k1",
    "k2",
    "k3",
    "k4",
    "values",
    "type",
    "tableValues",
    "slope",
    "intercept",
    "amplitude",
    "exponent",
    "stdDeviation",
    "flood-color",
    "flood-opacity",
    "radius",
    "role",
    "aria-label",
    "aria-hidden",
    "vector-effect",
    "shape-rendering",
    "text-rendering",
    "paint-order",
    "mix-blend-mode",
];

/// A cleaned drawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleaned {
    /// The drawing's markup, safe to place in the page.
    pub svg: String,
    /// What was dropped, deduplicated: `element script`, `attribute onload`.
    pub dropped: Vec<String>,
}

/// Cleans `source`, giving every id the prefix `prefix` (e.g. `d3-`).
pub fn clean(source: &str, prefix: &str) -> Result<Cleaned, String> {
    if source.len() > MAX_SVG_BYTES {
        return Err(format!(
            "the drawing is over {} MiB",
            MAX_SVG_BYTES / 1024 / 1024
        ));
    }
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: 200_000,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(source, options)
        .map_err(|e| format!("the drawing isn't valid SVG: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "svg" || !in_svg(root) {
        return Err("a drawing must be one <svg> element".to_owned());
    }
    let mut out = String::with_capacity(source.len());
    let mut dropped = BTreeSet::new();
    write_node(root, prefix, &mut out, &mut dropped, true);
    Ok(Cleaned {
        svg: out,
        dropped: dropped.into_iter().collect(),
    })
}

fn in_svg(node: roxmltree::Node<'_, '_>) -> bool {
    matches!(node.tag_name().namespace(), None | Some(SVG_NS))
}

fn write_node(
    node: roxmltree::Node<'_, '_>,
    prefix: &str,
    out: &mut String,
    dropped: &mut BTreeSet<String>,
    root: bool,
) {
    let name = node.tag_name().name();
    if !in_svg(node) || !ELEMENTS.contains(&name) {
        dropped.insert(format!("element {name}"));
        return;
    }
    out.push('<');
    out.push_str(name);
    if root {
        let _ = write!(out, " xmlns=\"{SVG_NS}\"");
    }
    for attr in node.attributes() {
        let local = attr.name();
        let value = attr.value();
        let kept = match attr.namespace() {
            None | Some(XLINK_NS) if local == "href" => href(value, prefix),
            None if ATTRIBUTES.contains(&local) => attribute(local, value, prefix),
            _ => None,
        };
        if let Some(value) = kept {
            let _ = write!(out, " {local}=\"{}\"", crate::escape(&value));
        } else {
            let shown = match attr.namespace() {
                Some(XLINK_NS) => format!("xlink:{local}"),
                _ => local.to_owned(),
            };
            dropped.insert(format!("attribute {shown}"));
        }
    }
    out.push('>');
    for child in node.children() {
        if child.is_element() {
            write_node(child, prefix, out, dropped, false);
        } else if child.is_text() {
            out.push_str(&crate::escape(child.text().unwrap_or_default()));
        }
    }
    let _ = write!(out, "</{name}>");
}

/// `href`: only a reference to an id in the drawing, prefixed.
fn href(value: &str, prefix: &str) -> Option<String> {
    let id = value.trim().strip_prefix('#')?;
    valid_id(id).then(|| format!("#{prefix}{id}"))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// A kept attribute's value, rewritten, or `None` to drop it.
fn attribute(name: &str, value: &str, prefix: &str) -> Option<String> {
    if name == "id" {
        return valid_id(value).then(|| format!("{prefix}{value}"));
    }
    let lower = value.to_ascii_lowercase();
    if value.contains('\\') || lower.contains("expression") || lower.contains('@') {
        return None;
    }
    rewrite_urls(value, prefix)
}

/// Rewrites every `url(#id)` to the prefixed id; `None` if any `url(` points elsewhere.
fn rewrite_urls(value: &str, prefix: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let Some(at) = rest.to_ascii_lowercase().find("url(") else {
            out.push_str(rest);
            return Some(out);
        };
        out.push_str(&rest[..at]);
        let after = &rest[at + 4..];
        let close = after.find(')')?;
        let inner = after[..close]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        let id = inner.strip_prefix('#')?;
        if !valid_id(id) {
            return None;
        }
        let _ = write!(out, "url(#{prefix}{id})");
        rest = &after[close + 1..];
    }
}
