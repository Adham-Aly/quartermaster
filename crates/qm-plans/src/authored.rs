//! Strict, inert HTML authoring contract. Source ranges are retained, never HTML-serialized.
use crate::markdown::{self, Parsed};
use crate::{Assets, Checked, Meta, Problem, Problems, Rendered, Rules, Section, escape};
use qm_types::domain::PlanMode;
use std::collections::BTreeSet;

/// Starter for `plan.html`: an XML-well-formed HTML fragment, not a full HTML document.
/// Edit the prose and layout freely within the documented inert element/attribute allow-list.
/// The app wraps this fragment; keep the authored source separate from the composed output.
pub const fn authoring_template() -> &'static str {
    include_str!("../template/authored.html")
}

/// Validate authored HTML and enforce the same round/mode rules as legacy Markdown.
/// Errors name `plan.html` in their message (line is intentionally unset).
pub fn check_html(html: &str, assets: &Assets, rules: &Rules) -> Result<Checked, Problems> {
    let parsed = parse(html, assets, rules.mode)?;
    crate::check_rules(parsed.checked, rules).map_err(html_problems)
}

/// Compose authored HTML with trusted metadata, section controls, questions, and runtime.
/// Like `render`, this checks structural safety; call `check_html` for round/mode policy first.
/// Never overwrite the authored source with this output: persist it separately for refreshes.
pub fn compose_html(html: &str, assets: &Assets, meta: &Meta) -> Result<Rendered, Problems> {
    let parsed = parse(html, assets, PlanMode::Interactive)?;
    Ok(Rendered {
        html: crate::page::assemble(&parsed, meta),
        checked: parsed.checked,
    })
}

fn fail(message: impl Into<String>) -> Problems {
    Problems(vec![Problem::whole(format!(
        "plan.html: {}",
        message.into()
    ))])
}

fn html_problems(problems: Problems) -> Problems {
    Problems(
        problems
            .0
            .into_iter()
            .map(|p| Problem::whole(format!("plan.html: {}", p.message)))
            .collect(),
    )
}

fn text(node: roxmltree::Node<'_, '_>) -> String {
    node.descendants()
        .filter(roxmltree::Node::is_text)
        .filter_map(|n| n.text())
        .collect()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn parse(html: &str, assets: &Assets, mode: PlanMode) -> Result<Parsed, Problems> {
    if html.len() > crate::MAX_PLAN_BYTES {
        return Err(fail("document exceeds 256 KiB"));
    }
    // No declarations, DTDs, entities or processing instructions. XML parsing alone is not a
    // browser sanitizer: prohibit namespaces, active elements and parser differential contexts.
    if html.contains("<!") || html.contains("<?") {
        return Err(fail("declarations and comments are not supported"));
    }
    let doc = roxmltree::Document::parse(html)
        .map_err(|e| fail(format!("use well-formed HTML (XML syntax): {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "article" {
        return Err(fail("the single root must be <article>"));
    }
    let mut checked = Checked::default();
    let mut edits = Vec::new();
    let mut ids = BTreeSet::new();
    let mut qids = BTreeSet::new();
    let mut uses_math = false;
    let mut section = None;
    let mut starts = Vec::new();
    let mut drawing = 0;
    for node in root.descendants().filter(roxmltree::Node::is_element) {
        let tag = node.tag_name().name();
        validate_node(node, &mut ids)?;
        add_heading(node, &mut checked, &mut section, &mut starts, &mut edits)?;
        if matches!(tag, "img" | "br" | "hr") && node.children().next().is_some() {
            return Err(fail("void elements must be empty and self-closing"));
        }
        add_image(node, assets, mode, &mut checked, &mut drawing, &mut edits)?;
        let kind = if node.has_attribute("data-question") {
            Some("question")
        } else {
            node.attribute("data-render")
        };
        if let Some(kind) = kind {
            let mut parsed = parse_placeholder(node, kind, assets, mode)?;
            for q in &mut parsed.checked.questions {
                if !qids.insert(q.id.clone()) {
                    return Err(fail("duplicate question id"));
                }
                if section.is_none() {
                    return Err(fail("questions must follow an h2/h3"));
                }
                q.section.clone_from(&section);
            }
            // Each placeholder is parsed independently: fix its displayed ordinal.
            if kind == "question" {
                parsed.body = parsed.body.replacen(
                    "class=\"q-num\">Q1</span>",
                    &format!("class=\"q-num\">Q{}</span>", checked.questions.len() + 1),
                    1,
                );
            }
            checked.questions.extend(parsed.checked.questions);
            if parsed.checked.diagrams > 0 {
                drawing += 1;
                parsed.body = parsed.body.replace("d1-", &format!("qm-d{drawing}-"));
            }
            checked.diagrams += parsed.checked.diagrams;
            checked.mockups += parsed.checked.mockups;
            checked.notes.extend(parsed.checked.notes);
            uses_math |= parsed.uses_math;
            edits.push((node.range(), parsed.body));
        }
    }
    if checked.title.is_none() {
        return Err(fail("add one h1 title"));
    }
    if checked.questions.len() > crate::MAX_QUESTIONS {
        return Err(fail("too many questions (maximum 60)"));
    }
    for (i, sec) in checked.sections.iter_mut().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(html.len());
        sec.fingerprint = crate::fingerprint(
            std::iter::once(&html.as_bytes()[starts[i]..end])
                .chain(assets.iter().map(|(_, bytes)| bytes)),
        );
    }
    // Descending offsets preserve every authored byte outside a runtime placeholder.
    edits.sort_by_key(|a| std::cmp::Reverse(a.0.start));
    let mut body = html.to_owned();
    for (range, replacement) in edits {
        body.replace_range(range, &replacement);
    }
    Ok(Parsed {
        checked,
        body,
        uses_math,
    })
}

fn validate_node<'a>(
    node: roxmltree::Node<'a, '_>,
    ids: &mut BTreeSet<&'a str>,
) -> Result<(), Problems> {
    let tag = node.tag_name().name();
    if node.tag_name().namespace().is_some() || node.namespaces().len() > 0 {
        return Err(fail("namespaces are not supported"));
    }
    if !ELEMENTS.contains(&tag) {
        return Err(fail(format!("element <{tag}> is not supported")));
    }
    for attr in node.attributes() {
        let name = attr.name();
        if attr.namespace().is_some() || !ATTRIBUTES.contains(&name) {
            return Err(fail(format!("attribute {name} is not supported")));
        }
        if name == "id"
            && (!valid_id(attr.value()) || !ids.insert(attr.value()) || !matches!(tag, "h2" | "h3"))
        {
            return Err(fail(
                "unique lowercase ids are allowed only on h2/h3 headings",
            ));
        }
        if name == "id"
            && (attr.value().starts_with("q-")
                || attr.value().starts_with("qm-")
                || RUNTIME_IDS.contains(&attr.value()))
        {
            return Err(fail("heading id is reserved by the runtime"));
        }
        if name == "class"
            && attr
                .value()
                .split_whitespace()
                .any(|c| !c.starts_with("author-"))
        {
            return Err(fail("authored class names must start with author-"));
        }
        if name == "href"
            && (tag != "a"
                || !["https://", "http://", "mailto:", "#"]
                    .iter()
                    .any(|p| attr.value().starts_with(p))
                || attr.value().chars().any(char::is_control))
        {
            return Err(fail("links must use http, https, mailto or #anchor"));
        }
        if name == "src" && tag != "img" {
            return Err(fail("src is allowed only on images"));
        }
        if matches!(name, "data-question" | "data-render") && tag != "pre" {
            return Err(fail("runtime placeholders must be pre elements"));
        }
    }
    Ok(())
}

fn add_heading(
    node: roxmltree::Node<'_, '_>,
    checked: &mut Checked,
    section: &mut Option<String>,
    starts: &mut Vec<usize>,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) -> Result<(), Problems> {
    let tag = node.tag_name().name();
    if matches!(tag, "h1" | "h2" | "h3") {
        if node.children().any(|n| n.is_element()) {
            return Err(fail("headings must contain plain text"));
        }
        let title = text(node);
        if title.trim().is_empty() {
            return Err(fail("headings must not be empty"));
        }
        if tag == "h1" {
            if checked.title.replace(title).is_some() {
                return Err(fail("use exactly one h1"));
            }
        } else {
            let id = node
                .attribute("id")
                .ok_or_else(|| fail("each h2/h3 needs a stable id"))?;
            *section = Some(id.to_owned());
            starts.push(node.range().start);
            checked.sections.push(Section {
                id: id.to_owned(),
                title: title.clone(),
                level: if tag == "h2" { 2 } else { 3 },
                fingerprint: String::new(),
            });
            let tools = format!(
                "<span class=\"badge-updated\" data-updated=\"{}\" hidden>updated</span><div class=\"sec-tools\" data-section=\"{}\" data-title=\"{}\"><button type=\"button\" class=\"link sec-change\">✎ Suggest a change</button><button type=\"button\" class=\"link sec-ask\">? Ask</button></div>",
                escape(id),
                escape(id),
                escape(&title)
            );
            edits.push((node.range().end..node.range().end, tools));
        }
    }
    Ok(())
}

fn add_image(
    node: roxmltree::Node<'_, '_>,
    assets: &Assets,
    mode: PlanMode,
    checked: &mut Checked,
    drawing: &mut usize,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) -> Result<(), Problems> {
    let tag = node.tag_name().name();
    if tag == "img" {
        let src = node
            .attribute("src")
            .ok_or_else(|| fail("images need src=assets/name"))?;
        // Delegate asset path checking, SVG cleaning and embedding to the existing renderer.
        if !src.starts_with("assets/")
            || !src[7..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(fail("invalid asset path: use assets/name.ext"));
        }
        let alt = node.attribute("alt").unwrap_or("");
        let mut parsed =
            markdown::parse(&format!("![image]({src})"), assets, mode).map_err(html_problems)?;
        parsed.body = parsed
            .body
            .replace("alt=\"image\"", &format!("alt=\"{}\"", escape(alt)));
        checked.images += parsed.checked.images;
        if parsed.checked.diagrams > 0 {
            *drawing += 1;
            parsed.body = parsed.body.replace("d1-", &format!("qm-d{drawing}-"));
        }
        checked.diagrams += parsed.checked.diagrams;
        checked.notes.extend(parsed.checked.notes);
        edits.push((node.range(), parsed.body));
    }
    Ok(())
}

const ELEMENTS: &[&str] = &[
    "article",
    "section",
    "div",
    "span",
    "h1",
    "h2",
    "h3",
    "h4",
    "p",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "strong",
    "em",
    "b",
    "i",
    "s",
    "small",
    "blockquote",
    "pre",
    "code",
    "a",
    "img",
    "br",
    "hr",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "figure",
    "figcaption",
    "details",
    "summary",
];

const ATTRIBUTES: &[&str] = &[
    "id",
    "class",
    "style",
    "title",
    "aria-label",
    "href",
    "src",
    "alt",
    "width",
    "height",
    "colspan",
    "rowspan",
    "data-question",
    "data-render",
];

const RUNTIME_IDS: &[&str] = &[
    "theme-toggle",
    "print",
    "reply-bar",
    "reply-count",
    "send-status",
    "take-recommended",
    "clear",
    "copy",
    "send",
    "copy-status",
    "copy-fallback",
    "copy-text",
    "copy-close",
    "plan-meta",
];

fn parse_placeholder(
    node: roxmltree::Node<'_, '_>,
    kind: &str,
    assets: &Assets,
    mode: PlanMode,
) -> Result<Parsed, Problems> {
    if node.children().any(|n| n.is_element())
        || node.has_attribute("data-question") && node.has_attribute("data-render")
    {
        return Err(fail(
            "placeholder must contain only escaped text and one directive",
        ));
    }
    if ![
        "question", "svg", "mockup", "math", "terminal", "rust", "json", "text",
    ]
    .contains(&kind)
    {
        return Err(fail("unsupported data-render kind"));
    }
    let content = text(node);
    if content.contains("```") {
        return Err(fail("placeholder may not contain Markdown fences"));
    }
    markdown::parse(&format!("```{kind}\n{content}\n```"), assets, mode).map_err(html_problems)
}
