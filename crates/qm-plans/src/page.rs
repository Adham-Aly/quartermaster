//! The page around the plan's body: the fixed template (`template/plan.html`), its stylesheet and
//! scripts, the header and banner for the plan's state, the outline, the data the script reads,
//! and a content security policy that lets only our own scripts run.

use crate::markdown::Parsed;
use crate::{Meta, PageState, escape, highlight, vendor};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

const TEMPLATE: &str = include_str!("../template/plan.html");
const CSS: &str = include_str!("../template/plan.css");
const THEME_JS: &str = include_str!("../template/theme.js");
pub(crate) const PLAN_JS: &str = include_str!("../template/plan.js");

/// The whole page.
pub(crate) fn assemble(parsed: &Parsed, meta: &Meta) -> String {
    let checked = &parsed.checked;
    let mut body = parsed.body.clone();
    for section in checked.updated(&meta.previous) {
        let hidden = format!("data-updated=\"{}\" hidden", escape(&section.id));
        let shown = format!("data-updated=\"{}\"", escape(&section.id));
        body = body.replacen(&hidden, &shown, 1);
    }
    let title = checked
        .title
        .clone()
        .unwrap_or_else(|| format!("Plan for {}", meta.task));

    let mut scripts = vec![PLAN_JS.to_owned()];
    let mut styles = vec![CSS.to_owned(), highlight::css().to_owned()];
    if parsed.uses_math {
        // KaTeX goes first so the page's script finds it.
        scripts.insert(
            0,
            format!(
                "/*! KaTeX {} | MIT licence:\n{}*/\n{}",
                "0.16.47",
                vendor::KATEX_LICENCE.replace("*/", "* /"),
                vendor::KATEX_JS
            ),
        );
        styles.push(vendor::katex_css().to_owned());
    }
    let mut hashes = vec![script_hash(THEME_JS)];
    hashes.extend(scripts.iter().map(|s| script_hash(s)));
    // The page reaches nothing but the app's own endpoint, and only while it takes replies.
    let connect = meta
        .sends_to()
        .map_or_else(|| "'none'".to_owned(), crate::Submit::origin);
    let csp = format!(
        "default-src 'none'; script-src {}; style-src 'unsafe-inline'; img-src data:; \
         font-src data:; connect-src {connect}; form-action 'none'; base-uri 'none'",
        hashes.join(" ")
    );

    let head = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">\n<title>{}</title>\n<style>\n{}</style>\n<script>{THEME_JS}</script>",
        escape(&csp),
        escape(&title),
        styles.join("\n")
    );
    let mut runtime = String::new();
    for script in &scripts {
        let _ = writeln!(runtime, "<script>{script}</script>");
    }

    fill(
        TEMPLATE,
        &[
            ("head", head),
            ("header", header(&title, meta, checked.questions.len())),
            ("banner", banner(meta, checked.questions.len())),
            ("outline", outline(parsed)),
            ("body", body),
            ("reply_buttons", reply_buttons(meta)),
            ("meta_json", meta_json(parsed, meta)),
            ("runtime", runtime),
        ],
    )
}

/// `'sha256-…'` for an inline script's exact text.
fn script_hash(script: &str) -> String {
    format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script.as_bytes()))
    )
}

/// Fills `{{name}}` slots in one pass over the template, so text that lands in one slot is never
/// read as another slot.
fn fill(template: &str, slots: &[(&str, String)]) -> String {
    let mut out =
        String::with_capacity(template.len() + slots.iter().map(|s| s.1.len()).sum::<usize>());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match slots.iter().find(|(n, _)| *n == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[open..open + 2 + close + 2]),
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

fn state_word(state: &PageState) -> &'static str {
    match state {
        PageState::Open => "open",
        PageState::Final => "final",
        PageState::Approved { .. } => "approved",
        PageState::Locked { .. } => "locked",
    }
}

fn header(title: &str, meta: &Meta, questions: usize) -> String {
    let status = match &meta.state {
        PageState::Open => format!(
            "{questions} question{} open",
            if questions == 1 { "" } else { "s" }
        ),
        PageState::Final => "Final".to_owned(),
        PageState::Approved { .. } => "Approved".to_owned(),
        PageState::Locked { .. } => "Closed to replies".to_owned(),
    };
    format!(
        "<h1 class=\"page-title\">{}</h1>\n<p class=\"page-meta\"><span>{}</span> · <span>round {}</span> · <span>{}</span> · <span class=\"status status-{}\">{}</span></p>",
        escape(title),
        escape(&meta.task),
        meta.round,
        escape(&meta.project),
        state_word(&meta.state),
        escape(&status)
    )
}

fn banner(meta: &Meta, questions: usize) -> String {
    let sends = meta.sends_to().is_some();
    let text = match &meta.state {
        PageState::Open if sends => format!(
            "Answer the {} below, then <b>Send to Quartermaster</b>. Or <b>Copy reply</b> and paste it into the Quartermaster's chat. Use ✎ on any section to change it or ? to ask about it.",
            if questions == 1 { "question" } else { "questions" }
        ),
        PageState::Open => format!(
            "Answer the {} below, then <b>Copy reply</b> and paste it into the Quartermaster's chat. Use ✎ on any section to change it or ? to ask about it.",
            if questions == 1 { "question" } else { "questions" }
        ),
        PageState::Final if sends => "No questions open. Approve the plan on its card in the Quartermaster's chat, or tell the Quartermaster “I approve the plan”. To change anything first, use ✎ on a section, then <b>Send to Quartermaster</b>, or <b>Copy reply</b> and paste it.".to_owned(),
        PageState::Final => "No questions open. Approve the plan on its card in the Quartermaster's chat, or tell the Quartermaster “I approve the plan”. To change anything first, use ✎ on a section, then <b>Copy reply</b> and paste it.".to_owned(),
        PageState::Approved { at } => format!("Approved {}.", escape(at)),
        PageState::Locked { reason } => escape(reason),
    };
    format!(
        "<div class=\"banner banner-{}\" role=\"status\">{text}</div>",
        state_word(&meta.state)
    )
}

/// The reply bar's buttons: Copy reply alone, or Copy reply beside the primary Send to
/// Quartermaster when the page may send.
fn reply_buttons(meta: &Meta) -> String {
    if meta.sends_to().is_some() {
        "<button type=\"button\" id=\"copy\" class=\"tool\">Copy reply</button>\n\
         <button type=\"button\" id=\"send\" class=\"primary\">Send to Quartermaster</button>"
            .to_owned()
    } else {
        "<button type=\"button\" id=\"copy\" class=\"primary\">Copy reply</button>".to_owned()
    }
}

/// The sticky outline of h2/h3, with a dot on sections that hold questions or changed.
fn outline(parsed: &Parsed) -> String {
    let checked = &parsed.checked;
    let mut html = String::from("<nav class=\"outline\" aria-label=\"Outline\"><ol>\n");
    for section in &checked.sections {
        let asks = checked
            .questions
            .iter()
            .filter(|q| q.section.as_deref() == Some(section.id.as_str()))
            .count();
        let dot = if asks > 0 {
            format!("<span class=\"dot\" title=\"{asks} open\"></span>")
        } else {
            String::new()
        };
        let _ = writeln!(
            html,
            "<li class=\"l{}\"><a href=\"#{}\">{}</a>{dot}</li>",
            section.level,
            escape(&section.id),
            escape(&section.title)
        );
    }
    html.push_str("</ol></nav>");
    html
}

/// The data the page's script reads, in a JSON block no character of which can end the element.
fn meta_json(parsed: &Parsed, meta: &Meta) -> String {
    let checked = &parsed.checked;
    let questions: Vec<serde_json::Value> = checked
        .questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            serde_json::json!({
                "id": q.id,
                "section": q.section,
                "n": i + 1,
                "multiple": q.multiple,
                "other": q.offers_other(),
                "open": q.choices.is_empty(),
                "choices": q.choices.iter().map(|c| serde_json::json!({
                    "id": c.id, "label": c.label, "recommended": c.recommended,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let sections: Vec<serde_json::Value> = checked
        .sections
        .iter()
        .map(|s| serde_json::json!({ "id": s.id, "title": s.title }))
        .collect();
    let mut value = serde_json::json!({
        "v": 1,
        "task": meta.task,
        "plan": meta.plan_uid,
        "round": meta.round,
        "state": state_word(&meta.state),
        "questions": questions,
        "sections": sections,
    });
    if let Some(submit) = meta.sends_to() {
        value["submit"] = serde_json::json!({ "url": submit.url(), "token": submit.token });
    }
    let text = value.to_string();
    let mut safe = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => safe.push_str("\\u003c"),
            '>' => safe.push_str("\\u003e"),
            '&' => safe.push_str("\\u0026"),
            '\u{2028}' => safe.push_str("\\u2028"),
            '\u{2029}' => safe.push_str("\\u2029"),
            c => safe.push(c),
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_fill_in_one_pass() {
        let out = fill(
            "a {{x}} b {{y}} {{z}}",
            &[("x", "{{y}}".to_owned()), ("y", "Y".to_owned())],
        );
        assert_eq!(out, "a {{y}} b Y {{z}}");
    }
}
