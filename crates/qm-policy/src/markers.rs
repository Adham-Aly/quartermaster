//! Marker neutralising.
//!
//! "Three sources an agent tells apart: its supervisor (marked), the app (marked), and you
//! (unmarked, and authoritative). The TUI neutralises the reserved `<qm:` prefix in anything you
//! type, so a marker can only come from the app."
//!
//! [`neutralise`] is applied to everything you type before it reaches an agent, and to any other
//! text the app passes on unmarked. It also covers closing tags (`</qm:`), so your text can't end
//! an app marker early, and matches case-insensitively.
//!
//! Neutralising replaces the `<` of each reserved prefix with `‹` (U+2039), which keeps the text
//! readable and can't form a marker.

/// The reserved prefix.
pub const PREFIX: &str = "<qm:";

/// What a neutralised `<` becomes.
pub const NEUTRAL: char = '\u{2039}';

fn reserved_at(bytes: &[u8], i: usize) -> Option<usize> {
    let rest = &bytes[i..];
    let after_lt = if rest.first() == Some(&b'<') {
        1
    } else {
        return None;
    };
    let after_slash = if rest.get(after_lt) == Some(&b'/') {
        after_lt + 1
    } else {
        after_lt
    };
    let tag = rest.get(after_slash..after_slash + 3)?;
    tag.eq_ignore_ascii_case(b"qm:").then_some(after_slash + 3)
}

/// Whether text contains a reserved prefix (`<qm:` or `</qm:`, any case).
pub fn contains_marker(text: &str) -> bool {
    let b = text.as_bytes();
    (0..b.len()).any(|i| reserved_at(b, i).is_some())
}

/// Neutralises every reserved prefix, so the text can never open or close a marker.
pub fn neutralise(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for i in 0..b.len() {
        if i >= last && reserved_at(b, i).is_some() {
            out.push_str(&text[last..i]);
            out.push(NEUTRAL);
            last = i + 1;
        }
    }
    out.push_str(&text[last..]);
    out
}
