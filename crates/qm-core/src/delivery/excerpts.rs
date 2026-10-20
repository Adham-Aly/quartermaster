//! Text from outside (CI logs, check names, GitHub content) on its way to an agent: neutralised, so
//! it can never open or close a marker, and cut to size.
//!
//! The rule is `qm_policy::markers::neutralise` plus a cut with "…".

use qm_policy::markers::neutralise;

/// The most characters of excerpts one `checks` message carries in all.
pub const EXCERPTS_CHARS: usize = 12_000;
/// What ends a `checks` message whose excerpts were cut.
pub const EXCERPTS_CUT: &str = "… read the rest with ci_results";

/// Neutralises `raw` and cuts it to `cap` characters, ending with "…" when cut.
pub fn external(raw: &str, cap: usize) -> String {
    let text = neutralise(raw);
    if text.chars().count() <= cap {
        return text;
    }
    let mut out: String = text.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A check's name as an agent sees it: neutralised, one line, at most 100 characters.
pub fn check_name(raw: &str) -> String {
    external(&raw.replace(['\n', '\r'], " "), 100)
}

/// `## name\nexcerpt` sections appended to `head` while they fit in [`EXCERPTS_CHARS`] in all;
/// past that, [`EXCERPTS_CUT`].
pub fn with_excerpts(head: &str, excerpts: &[(String, String)]) -> String {
    let mut out = head.to_owned();
    let mut used = 0usize;
    for (name, excerpt) in excerpts {
        let section = format!(
            "\n\n## {}\n{}",
            check_name(name),
            external(excerpt, EXCERPTS_CHARS)
        );
        let n = section.chars().count();
        if used + n > EXCERPTS_CHARS {
            let room = EXCERPTS_CHARS.saturating_sub(used);
            if room > 0 {
                out.extend(section.chars().take(room));
            }
            out.push_str("\n\n");
            out.push_str(EXCERPTS_CUT);
            return out;
        }
        used += n;
        out.push_str(&section);
    }
    out
}
