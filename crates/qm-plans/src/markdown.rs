//! `plan.md` to the page's body. Markdown through `pulldown-cmark` (tables, footnotes,
//! strikethrough, task lists, `{#id}` heading attributes, `$math$`, GFM alerts), with our rules:
//!
//! | Source | Becomes |
//! |---|---|
//! | `##`/`###` | A section: a stable id (`{#id}`, else a slug, `-2` for repeats), its fingerprint, its "Suggest a change / Ask" tools |
//! | ```` ```lang ```` | Highlighted `<pre>` (unknown languages plain) |
//! | `$x$`, `$$x$$`, ```` ```math ```` | TeX for KaTeX, as escaped text |
//! | ```` ```svg ```` | The drawing inline, through [`crate::svg`] |
//! | ```` ```mockup ```` | An HTML/CSS mockup in a sandboxed `iframe` that can't run scripts |
//! | ```` ```terminal ```` | A terminal frame; ANSI colour codes become the 16 terminal colours |
//! | ```` ```question ```` | A question widget (interactive), an error (internal) |
//! | ```` ```mermaid ```` | An error: diagrams are SVG |
//! | `![alt](assets/x.png)` | The image embedded as `data:` (SVG inline); only files in `assets/` |
//! | `[t](https://…)` | A link opening a new tab; only `http`, `https`, `mailto` and `#…` |
//! | Raw HTML | Shown as text |

use crate::question::{self, Question};
use crate::{Assets, Checked, Problem, Problems, Section, escape, highlight, svg};
use base64::Engine as _;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use qm_types::domain::PlanMode;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

/// A parsed plan: what it holds, and its body as HTML.
#[derive(Clone, Debug)]
pub(crate) struct Parsed {
    pub checked: Checked,
    pub body: String,
    pub uses_math: bool,
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM
}

/// Parses `md`; every problem is collected before failing.
pub(crate) fn parse(md: &str, assets: &Assets, mode: PlanMode) -> Result<Parsed, Problems> {
    if md.len() > crate::MAX_PLAN_BYTES {
        return Err(Problems(vec![Problem::whole(format!(
            "plan.md is over {} KiB; keep the plan to what the work needs",
            crate::MAX_PLAN_BYTES / 1024
        ))]));
    }
    let events: Vec<(Event<'_>, Range<usize>)> =
        Parser::new_ext(md, options()).into_offset_iter().collect();
    let mut state = State {
        md,
        assets,
        mode,
        lines: md.match_indices('\n').map(|(i, _)| i).collect(),
        problems: Vec::new(),
        notes: Vec::new(),
        title: None,
        questions: Vec::new(),
        heading_ids: BTreeMap::new(),
        section_starts: Vec::new(),
        image_refs: Vec::new(),
        uses_math: false,
        drawings: 0,
        diagrams: 0,
        mockups: 0,
        images: 0,
    };
    let out = state.transform(&events, false);
    let mut body = String::with_capacity(md.len() * 2);
    pulldown_cmark::html::push_html(&mut body, out.into_iter());
    if state.questions.len() > crate::MAX_QUESTIONS {
        state.problems.push(Problem::whole(format!(
            "{} questions in one round; at most {} — ask the ones that decide the most first",
            state.questions.len(),
            crate::MAX_QUESTIONS
        )));
    }
    if !state.problems.is_empty() {
        state.problems.sort_by_key(|p| p.line.unwrap_or(usize::MAX));
        return Err(Problems(state.problems));
    }
    let sections = state.sections();
    Ok(Parsed {
        checked: Checked {
            title: state.title,
            questions: state.questions,
            sections,
            images: state.images,
            diagrams: state.diagrams,
            mockups: state.mockups,
            notes: state.notes,
        },
        body,
        uses_math: state.uses_math,
    })
}

struct State<'a> {
    md: &'a str,
    assets: &'a Assets,
    mode: PlanMode,
    /// Byte offsets of every newline, for line numbers.
    lines: Vec<usize>,
    problems: Vec<Problem>,
    notes: Vec<Problem>,
    title: Option<String>,
    questions: Vec<Question>,
    /// Every heading id so far, with how often it was asked for.
    heading_ids: BTreeMap<String, usize>,
    /// h2/h3 sections: (start offset, level, id, title).
    section_starts: Vec<(usize, u8, String, String)>,
    /// (offset, asset name) of every image shown.
    image_refs: Vec<(usize, String)>,
    uses_math: bool,
    drawings: usize,
    diagrams: usize,
    mockups: usize,
    images: usize,
}

impl State<'_> {
    /// The 1-based line of a byte offset.
    fn line(&self, offset: usize) -> usize {
        self.lines.partition_point(|&nl| nl < offset) + 1
    }

    fn problem(&mut self, offset: usize, message: impl Into<String>) {
        let line = self.line(offset);
        self.problems.push(Problem::at(line, message));
    }

    /// A heading: an anchor, and on h2/h3 the "updated" badge and the section's buttons.
    fn heading<'e>(
        &mut self,
        inner: &[(Event<'e>, Range<usize>)],
        level: HeadingLevel,
        id: Option<&str>,
        start: usize,
        fragment: bool,
        out: &mut Vec<Event<'e>>,
    ) {
        let text = plain_text(inner);
        let level_n = heading_number(level);
        if fragment {
            out.push(Event::Html(
                format!("<p><strong>{}</strong></p>", escape(&text)).into(),
            ));
            return;
        }
        let wanted = match id {
            Some(explicit) if valid_heading_id(explicit) => explicit.to_owned(),
            Some(explicit) => {
                self.problem(
                    start,
                    format!("heading id {explicit:?} must be 1 to 64 of a-z, A-Z, 0-9, - and _"),
                );
                slug(&text)
            }
            None => slug(&text),
        };
        let id = self.unique_id(&wanted);
        if level_n == 1 && self.title.is_none() {
            self.title = Some(text.clone());
        }
        out.push(Event::Html(
            format!("<h{level_n} id=\"{}\">", escape(&id)).into(),
        ));
        out.extend(self.transform(inner, true));
        let section = matches!(level_n, 2 | 3);
        if section {
            out.push(Event::Html(
                format!(
                    "<span class=\"badge-updated\" data-updated=\"{}\" hidden>updated</span>",
                    escape(&id)
                )
                .into(),
            ));
            self.section_starts
                .push((start, level_n, id.clone(), text.clone()));
        }
        out.push(Event::Html(format!("</h{level_n}>\n").into()));
        if section {
            out.push(Event::Html(section_tools(&id, &text).into()));
        }
    }

    /// Our events for `events`. `fragment` is a question's text: no headings, images or blocks.
    fn transform<'e>(
        &mut self,
        events: &[(Event<'e>, Range<usize>)],
        fragment: bool,
    ) -> Vec<Event<'e>> {
        let mut out = Vec::with_capacity(events.len());
        let mut links: Vec<bool> = Vec::new();
        let mut i = 0;
        while i < events.len() {
            let (event, range) = &events[i];
            match event {
                Event::Start(Tag::Heading { level, id, .. }) => {
                    let end = matching_end(events, i);
                    self.heading(
                        &events[i + 1..end],
                        *level,
                        id.as_deref(),
                        range.start,
                        fragment,
                        &mut out,
                    );
                    i = end + 1;
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    let end = matching_end(events, i);
                    let code: String = events[i + 1..end]
                        .iter()
                        .filter_map(|(e, _)| match e {
                            Event::Text(t) => Some(t.as_ref()),
                            _ => None,
                        })
                        .collect();
                    let info = match kind {
                        CodeBlockKind::Fenced(info) => info.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    if fragment {
                        out.push(Event::Html(plain_pre(&code).into()));
                    } else {
                        let html = self.code_block(&info, &code, range.start);
                        out.push(Event::Html(html.into()));
                    }
                    i = end + 1;
                }
                Event::Start(Tag::Image { dest_url, .. }) => {
                    let end = matching_end(events, i);
                    let alt = plain_text(&events[i + 1..end]);
                    if fragment {
                        self.problem(
                            range.start,
                            "images go in the plan's body, not in a question",
                        );
                    } else {
                        let html = self.image(dest_url, &alt, range.start);
                        out.push(Event::Html(html.into()));
                    }
                    i = end + 1;
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let open = open_link(dest_url);
                    links.push(open.is_some());
                    out.extend(open.map(|html| Event::Html(html.into())));
                    i += 1;
                }
                Event::End(TagEnd::Link) => {
                    if links.pop() == Some(true) {
                        out.push(Event::Html("</a>".into()));
                    }
                    i += 1;
                }
                Event::Html(raw) | Event::InlineHtml(raw) => {
                    out.push(Event::Text(raw.clone()));
                    i += 1;
                }
                Event::InlineMath(tex) => {
                    self.uses_math = true;
                    out.push(Event::Html(math(tex, false).into()));
                    i += 1;
                }
                Event::DisplayMath(tex) => {
                    self.uses_math = true;
                    out.push(Event::Html(math(tex, true).into()));
                    i += 1;
                }
                other => {
                    out.push(other.clone());
                    i += 1;
                }
            }
        }
        out
    }

    /// The id for a heading that wants `wanted`, with `-2`, `-3` for repeats.
    fn unique_id(&mut self, wanted: &str) -> String {
        let count = self.heading_ids.entry(wanted.to_owned()).or_insert(0);
        *count += 1;
        if *count == 1 {
            wanted.to_owned()
        } else {
            let mut n = *count;
            loop {
                let candidate = format!("{wanted}-{n}");
                if !self.heading_ids.contains_key(&candidate) {
                    self.heading_ids.insert(candidate.clone(), 1);
                    return candidate;
                }
                n += 1;
            }
        }
    }

    fn code_block(&mut self, info: &str, code: &str, offset: usize) -> String {
        let mut words = info.split_whitespace();
        let lang = words.next().unwrap_or("").to_ascii_lowercase();
        match lang.as_str() {
            "question" => self.question(code, offset),
            "mermaid" => {
                self.problem(
                    offset,
                    "Mermaid isn't supported: draw the diagram as SVG in a ```svg block",
                );
                String::new()
            }
            "svg" => {
                self.diagrams += 1;
                self.drawing(code, "", offset)
            }
            "math" | "latex" | "tex" => {
                self.uses_math = true;
                math(code, true)
            }
            "mockup" => {
                self.mockups += 1;
                let height = words
                    .find_map(|w| w.strip_prefix("height="))
                    .and_then(|h| h.parse::<u32>().ok())
                    .map_or(320, |h| h.clamp(80, 2000));
                format!(
                    "<figure class=\"mockup\"><iframe sandbox=\"\" referrerpolicy=\"no-referrer\" \
                     title=\"Mockup\" style=\"height:{height}px\" srcdoc=\"{}\"></iframe></figure>\n",
                    escape(&mockup_document(code))
                )
            }
            "terminal" => {
                format!(
                    "<pre class=\"terminal\"><code>{}</code></pre>\n",
                    ansi(code)
                )
            }
            "" => plain_pre(code),
            lang => match highlight::code(lang, code) {
                Some(html) => format!(
                    "<pre class=\"code\" data-lang=\"{}\"><code>{html}</code></pre>\n",
                    escape(lang)
                ),
                None => plain_pre(code),
            },
        }
    }

    fn question(&mut self, body: &str, offset: usize) -> String {
        let line = self.line(offset);
        if self.mode == PlanMode::Internal {
            self.problem(
                offset,
                "autonomy is auto: decide this yourself with the option you'd recommend and record \
                 it under \"Decisions made without the user\"",
            );
            return String::new();
        }
        let mut question = match question::parse(body, line) {
            Ok(q) => q,
            Err(problems) => {
                self.problems.extend(problems);
                return String::new();
            }
        };
        if self.questions.iter().any(|q| q.id == question.id) {
            self.problem(
                offset,
                format!("question id {:?} is used twice", question.id),
            );
            return String::new();
        }
        question.section = self.section_starts.last().map(|s| s.2.clone());
        let number = self.questions.len() + 1;
        let html = self.widget(&question, number, offset);
        self.questions.push(question);
        html
    }

    /// A question's widget; the page's script makes it live.
    fn widget(&mut self, q: &Question, number: usize, offset: usize) -> String {
        let ask = self.fragment(&q.ask, offset);
        let why = q
            .why
            .as_deref()
            .map(|why| self.fragment(why, offset))
            .unwrap_or_default();
        let id = escape(&q.id);
        let mut html = format!(
            "<div class=\"q\" id=\"q-{id}\" data-q=\"{id}\">\n<div class=\"q-head\"><span class=\"q-num\">Q{number}</span><div class=\"q-ask\">{ask}</div></div>\n"
        );
        if !why.is_empty() {
            let _ = writeln!(html, "<div class=\"q-why\">{why}</div>");
        }
        if q.choices.is_empty() {
            html.push_str(
                "<textarea class=\"q-answer\" rows=\"3\" placeholder=\"Your answer\"></textarea>\n",
            );
        } else {
            let _ = writeln!(
                html,
                "<div class=\"q-choices\" role=\"group\" data-multiple=\"{}\">",
                q.multiple
            );
            for c in &q.choices {
                let _ = writeln!(
                    html,
                    "<button type=\"button\" class=\"choice\" data-choice=\"{}\" aria-pressed=\"false\"><span class=\"c-label\">{}</span>{}{}</button>",
                    escape(&c.id),
                    escape(&c.label),
                    if c.recommended {
                        "<span class=\"c-rec\">Recommended</span>"
                    } else {
                        ""
                    },
                    c.detail
                        .as_deref()
                        .map(|d| format!("<span class=\"c-detail\">{}</span>", escape(d)))
                        .unwrap_or_default()
                );
            }
            if q.offers_other() {
                html.push_str("<button type=\"button\" class=\"choice c-other\" data-choice=\"\" aria-pressed=\"false\"><span class=\"c-label\">Other…</span></button>\n");
            }
            html.push_str("</div>\n");
            if q.offers_other() {
                html.push_str("<textarea class=\"q-other\" rows=\"2\" placeholder=\"Your own answer\" hidden></textarea>\n");
            }
        }
        html.push_str("<button type=\"button\" class=\"link q-add-note\">Add a note</button>\n<textarea class=\"q-note\" rows=\"2\" placeholder=\"A note for the planner\" hidden></textarea>\n</div>\n");
        html
    }

    /// A question's Markdown text as HTML: links, code and math, no blocks of our own.
    fn fragment(&mut self, md: &str, offset: usize) -> String {
        let events: Vec<_> = Parser::new_ext(md, options())
            .into_offset_iter()
            .map(|(e, _)| (e, offset..offset))
            .collect();
        let out = self.transform(&events, true);
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, out.into_iter());
        html
    }

    fn image(&mut self, dest: &str, alt: &str, offset: usize) -> String {
        let name = dest.trim();
        let inside = name
            .strip_prefix("assets/")
            .is_some_and(|file| !file.is_empty() && !file.contains('/') && !file.starts_with('.'));
        if !inside {
            self.problem(
                offset,
                format!(
                    "image {name:?}: images must be files in the plan's assets folder, written as assets/<name>"
                ),
            );
            return String::new();
        }
        let Some(mime) = crate::image_type(name) else {
            self.problem(
                offset,
                format!("image {name:?}: use png, jpg, gif, webp or svg"),
            );
            return String::new();
        };
        let Some(bytes) = self.assets.get(name) else {
            self.problem(offset, format!("image {name:?} isn't in the assets folder"));
            return String::new();
        };
        self.image_refs.push((offset, name.to_owned()));
        if mime == "image/svg+xml" {
            self.diagrams += 1;
            let Ok(source) = std::str::from_utf8(bytes) else {
                self.problem(offset, format!("image {name:?} isn't UTF-8 SVG"));
                return String::new();
            };
            let source = source.to_owned();
            return self.drawing(&source, alt, offset);
        }
        if !magic_matches(mime, bytes) {
            self.problem(offset, format!("image {name:?} isn't a {mime} file"));
            return String::new();
        }
        self.images += 1;
        format!(
            "<figure class=\"image\"><img alt=\"{}\" src=\"data:{mime};base64,{}\">{}</figure>\n",
            escape(alt),
            base64::engine::general_purpose::STANDARD.encode(bytes),
            caption(alt)
        )
    }

    /// An inline drawing, cleaned.
    fn drawing(&mut self, source: &str, alt: &str, offset: usize) -> String {
        self.drawings += 1;
        let prefix = format!("d{}-", self.drawings);
        match svg::clean(source, &prefix) {
            Ok(cleaned) => {
                if !cleaned.dropped.is_empty() {
                    let line = self.line(offset);
                    self.notes.push(Problem::at(
                        line,
                        format!(
                            "the drawing lost what the page doesn't allow: {}",
                            cleaned.dropped.join(", ")
                        ),
                    ));
                }
                let label = if alt.is_empty() {
                    String::new()
                } else {
                    format!(" role=\"img\" aria-label=\"{}\"", escape(alt))
                };
                format!(
                    "<figure class=\"drawing\"{label}>{}{}</figure>\n",
                    cleaned.svg,
                    caption(alt)
                )
            }
            Err(e) => {
                self.problem(offset, e);
                String::new()
            }
        }
    }

    /// The h2/h3 sections with their fingerprints: the Markdown from the heading to the next
    /// h2/h3, plus the bytes of each asset shown in it.
    fn sections(&self) -> Vec<Section> {
        let mut sections = Vec::with_capacity(self.section_starts.len());
        for (k, (start, level, id, title)) in self.section_starts.iter().enumerate() {
            let end = self
                .section_starts
                .get(k + 1)
                .map_or(self.md.len(), |next| next.0);
            let mut parts: Vec<&[u8]> = vec![&self.md.as_bytes()[*start..end]];
            for (at, name) in &self.image_refs {
                if (*start..end).contains(at) {
                    parts.push(name.as_bytes());
                    parts.push(self.assets.get(name).unwrap_or_default());
                }
            }
            sections.push(Section {
                id: id.clone(),
                title: title.clone(),
                level: *level,
                fingerprint: crate::fingerprint(parts),
            });
        }
        sections
    }
}

/// The index of the `End` that closes the `Start` at `start`.
fn matching_end(events: &[(Event<'_>, Range<usize>)], start: usize) -> usize {
    let mut depth = 0usize;
    for (j, (event, _)) in events.iter().enumerate().skip(start) {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
    }
    events.len().saturating_sub(1)
}

fn plain_text(events: &[(Event<'_>, Range<usize>)]) -> String {
    let mut text = String::new();
    for (event, _) in events {
        match event {
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                text.push_str(t);
            }
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            _ => {}
        }
    }
    text.trim().to_owned()
}

const fn heading_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn valid_heading_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `3.2 Data flow!` → `32-data-flow`; empty → `section`.
pub(crate) fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else if c.is_whitespace() || c == '-' || c == '_' {
            dash = true;
        }
    }
    out.truncate(64);
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "section".to_owned()
    } else {
        out
    }
}

/// A section's "Suggest a change" and "Ask" controls; the page's script opens their boxes.
fn section_tools(id: &str, title: &str) -> String {
    format!(
        "<div class=\"sec-tools\" data-section=\"{}\" data-title=\"{}\"><button type=\"button\" class=\"link sec-change\">✎ Suggest a change</button><button type=\"button\" class=\"link sec-ask\">? Ask</button></div>\n",
        escape(id),
        escape(title)
    )
}

/// The opening `<a>` for a link the page allows, or `None` to show only its text.
fn open_link(dest: &str) -> Option<String> {
    match link_target(dest)? {
        LinkKind::Fragment => Some(format!("<a href=\"{}\">", escape(dest))),
        LinkKind::Outside => Some(format!(
            "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">",
            escape(dest)
        )),
    }
}

enum LinkKind {
    Fragment,
    Outside,
}

fn link_target(dest: &str) -> Option<LinkKind> {
    let dest = dest.trim();
    if dest.starts_with('#') {
        return Some(LinkKind::Fragment);
    }
    let lower = dest.to_ascii_lowercase();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
        .then_some(LinkKind::Outside)
}

fn math(tex: &str, display: bool) -> String {
    if display {
        format!(
            "<div class=\"math\" data-display=\"true\">{}</div>\n",
            escape(tex.trim())
        )
    } else {
        format!(
            "<span class=\"math\" data-display=\"false\">{}</span>",
            escape(tex)
        )
    }
}

fn plain_pre(code: &str) -> String {
    format!("<pre class=\"code\"><code>{}</code></pre>\n", escape(code))
}

fn caption(alt: &str) -> String {
    if alt.is_empty() {
        String::new()
    } else {
        format!("<figcaption>{}</figcaption>", escape(alt))
    }
}

/// A mockup's document: the planner's HTML and CSS, with a plain base style.
fn mockup_document(code: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><style>body{{margin:12px;font:14px -apple-system,system-ui,sans-serif}}</style>{code}"
    )
}

/// Whether `bytes` start like a `mime` file.
fn magic_matches(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// A terminal block's text with ANSI SGR colour codes as spans of the 16 terminal colours. An
/// escape may be written as the byte itself or as `\e[`, `\x1b[`, `\033[` or `^[[`.
fn ansi(code: &str) -> String {
    let mut text = code.to_owned();
    for spelled in ["\\e[", "\\x1b[", "\\x1B[", "\\033[", "^[["] {
        text = text.replace(spelled, "\u{1b}[");
    }
    let mut out = String::with_capacity(text.len());
    let mut open = false;
    let (mut fg, mut bg, mut bold, mut dim, mut underline) = (None, None, false, false, false);
    let mut rest = text.as_str();
    while let Some(at) = rest.find("\u{1b}[") {
        out.push_str(&escape(&rest[..at]));
        let after = &rest[at + 2..];
        let Some(end) = after.find(|c: char| !(c.is_ascii_digit() || c == ';')) else {
            rest = "";
            break;
        };
        let params = &after[..end];
        let command = after[end..].chars().next().unwrap_or(' ');
        rest = &after[end + command.len_utf8()..];
        if command != 'm' {
            continue;
        }
        for p in params.split(';') {
            match p.parse::<u8>().unwrap_or(0) {
                0 => (fg, bg, bold, dim, underline) = (None, None, false, false, false),
                1 => bold = true,
                2 => dim = true,
                4 => underline = true,
                22 => (bold, dim) = (false, false),
                24 => underline = false,
                n @ 30..=37 => fg = Some(n - 30),
                n @ 90..=97 => fg = Some(n - 90 + 8),
                39 => fg = None,
                n @ 40..=47 => bg = Some(n - 40),
                n @ 100..=107 => bg = Some(n - 100 + 8),
                49 => bg = None,
                _ => {}
            }
        }
        if open {
            out.push_str("</span>");
            open = false;
        }
        let mut classes = Vec::new();
        if let Some(n) = fg {
            classes.push(format!("t-f{n}"));
        }
        if let Some(n) = bg {
            classes.push(format!("t-b{n}"));
        }
        for (on, class) in [(bold, "t-bold"), (dim, "t-dim"), (underline, "t-u")] {
            if on {
                classes.push(class.to_owned());
            }
        }
        if !classes.is_empty() {
            let _ = write!(out, "<span class=\"{}\">", classes.join(" "));
            open = true;
        }
    }
    out.push_str(&escape(rest));
    if open {
        out.push_str("</span>");
    }
    out
}
