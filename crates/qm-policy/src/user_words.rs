//! Trimming source user words stored as task or routed-request provenance.
//! These stored words are not pasted into briefs or routed request messages.
//!
//! An orchestrator may leave out parts of the user's message that are about other work (another
//! task, another crewmate, what to do after something else finishes). The rule keeps that a pure
//! deletion of whole pieces, so the request itself can't be reworded:
//!
//! - each cut is quoted exactly from the message and appears in it exactly once;
//! - each cut is a whole sentence, line or clause: it starts at the start of the message, of a
//!   line, or after `.`, `!`, `?`, `;`, `:` or `,` (or begins with one of those), and ends at the
//!   end of the message, of a line, or at one of those marks (or ends with one);
//! - cuts don't overlap, and something of the message is left.
//!
//! Only whitespace at the joins is tidied; every kept character is the user's own.

use qm_types::error::Refusal;

/// The most passages one message may have cut.
pub const MAX_CUTS: usize = 12;

/// The marks a cut may start after or end at.
const BOUNDARY: [char; 6] = ['.', '!', '?', ';', ':', ','];

/// The shape of a list of cuts, before the message is known: how many, and none empty.
pub fn check_shape(cuts: &[String]) -> Result<(), Refusal> {
    if cuts.len() > MAX_CUTS {
        return Err(Refusal::new(format!(
            "words_cut takes at most {MAX_CUTS} passages (this one has {})",
            cuts.len()
        )));
    }
    if cuts.iter().any(|c| c.trim().is_empty()) {
        return Err(Refusal::new("a words_cut passage can't be empty"));
    }
    Ok(())
}

/// `original` with every passage in `cuts` left out, or why the cuts can't be applied.
pub fn trim(original: &str, cuts: &[String]) -> Result<String, Refusal> {
    check_shape(cuts)?;
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(cuts.len());
    for cut in cuts {
        let piece = cut.trim();
        let mut found = original.match_indices(piece).map(|(at, _)| at);
        let Some(start) = found.next() else {
            return Err(Refusal::new(format!(
                "words_cut passage \"{}\" isn't in the user's message word for word; copy it exactly",
                short(piece)
            )));
        };
        if found.next().is_some() {
            return Err(Refusal::new(format!(
                "words_cut passage \"{}\" appears more than once in the user's message; quote more of it",
                short(piece)
            )));
        }
        let end = start + piece.len();
        if !starts_at_boundary(original, start, piece) || !ends_at_boundary(original, end, piece) {
            return Err(Refusal::new(format!(
                "words_cut passage \"{}\" isn't a whole sentence, line or clause; cut only whole pieces about other work, never words inside the request",
                short(piece)
            )));
        }
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(Refusal::new(
            "words_cut passages overlap; name each piece once",
        ));
    }

    let mut kept = String::with_capacity(original.len());
    let mut from = 0;
    for (start, end) in ranges {
        push_joined(&mut kept, &original[from..start]);
        from = end;
    }
    push_joined(&mut kept, &original[from..]);
    let tidy = tidy(&kept);
    if tidy.is_empty() {
        return Err(Refusal::new(
            "words_cut would leave nothing of the user's message; choose the message that holds the request with words_message",
        ));
    }
    Ok(tidy)
}

/// Whether a cut of `piece` at `start` begins a sentence, line or clause.
fn starts_at_boundary(text: &str, start: usize, piece: &str) -> bool {
    if piece.starts_with(BOUNDARY) {
        return true;
    }
    match text[..start]
        .trim_end_matches([' ', '\t'])
        .chars()
        .next_back()
    {
        None | Some('\n' | '\r') => true,
        Some(c) => BOUNDARY.contains(&c) || is_list_mark(text, start),
    }
}

/// Whether a cut of `piece` ending at `end` ends a sentence, line or clause.
fn ends_at_boundary(text: &str, end: usize, piece: &str) -> bool {
    if piece.ends_with(BOUNDARY) {
        return true;
    }
    match text[end..].trim_start_matches([' ', '\t']).chars().next() {
        None | Some('\n' | '\r') => true,
        Some(c) => BOUNDARY.contains(&c),
    }
}

/// Whether the text before `start` on its line is only a list mark (`-`, `*`, `•`, `1.`, `2)`).
fn is_list_mark(text: &str, start: usize) -> bool {
    let line = text[..start].rsplit('\n').next().unwrap_or_default().trim();
    matches!(line, "-" | "*" | "•" | "+")
        || (line.len() > 1
            && (line.ends_with('.') || line.ends_with(')'))
            && line[..line.len() - 1].chars().all(|c| c.is_ascii_digit()))
}

/// Appends `piece` to `out`, keeping one space where a cut joined two runs of spaces.
fn push_joined(out: &mut String, piece: &str) {
    if out.ends_with([' ', '\t']) {
        out.push_str(piece.trim_start_matches([' ', '\t']));
    } else {
        out.push_str(piece);
    }
}

/// Trailing spaces off each line, at most one blank line in a row, no blank ends; a line left
/// holding only a list mark goes too.
fn tidy(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim_end) {
        let bare = line.trim();
        if matches!(bare, "-" | "*" | "•" | "+") {
            continue;
        }
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let joined = out.join("\n");
    let joined = joined.trim();
    // A clause cut at the very start can leave its own leading mark (", and …").
    joined
        .trim_start_matches([',', ';', ':'])
        .trim_start()
        .to_owned()
}

fn short(text: &str) -> String {
    const MAX: usize = 60;
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let mut s: String = one_line.chars().take(MAX - 1).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cuts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_cuts_keep_the_message_as_it_is() {
        assert_eq!(
            trim("Fix the login bug.", &[]).unwrap(),
            "Fix the login bug."
        );
    }

    #[test]
    fn whole_sentences_about_other_work_are_left_out() {
        let message = "Fix the login bug in the web app. After C-0042 is finished, start a \
                       review of project foobar. Keep the session cookie as it is.";
        let trimmed = trim(
            message,
            &cuts(&["After C-0042 is finished, start a review of project foobar."]),
        )
        .unwrap();
        assert_eq!(
            trimmed,
            "Fix the login bug in the web app. Keep the session cookie as it is."
        );
    }

    #[test]
    fn whole_lines_and_paragraphs_go_with_their_blank_lines() {
        let message = "Make the brief take the right message.\n\n\
                       - after the task for project ABC is done, spin up a crewmate for foobar\n\
                       - keep the user's wording\n\nThanks!";
        let trimmed = trim(
            message,
            &cuts(&["after the task for project ABC is done, spin up a crewmate for foobar"]),
        )
        .unwrap();
        assert_eq!(
            trimmed,
            "Make the brief take the right message.\n\n- keep the user's wording\n\nThanks!"
        );
    }

    #[test]
    fn a_clause_may_go_with_its_leading_comma() {
        let message = "Rename the setting, and once C-7 lands spin up a crewmate for docs.";
        let trimmed = trim(
            message,
            &cuts(&[", and once C-7 lands spin up a crewmate for docs"]),
        )
        .unwrap();
        assert_eq!(trimmed, "Rename the setting.");
    }

    #[test]
    fn words_inside_a_sentence_cant_be_cut() {
        let message = "Please don't delete the old files.";
        for cut in ["don't ", "don't", "old files", "Please don't"] {
            let refused = trim(message, &cuts(&[cut])).unwrap_err();
            assert!(
                refused.reason.contains("whole sentence"),
                "{cut}: {}",
                refused.reason
            );
        }
    }

    #[test]
    fn a_cut_must_be_quoted_exactly_and_once() {
        let message = "Do it. Do it.";
        assert!(
            trim(message, &cuts(&["Do it."]))
                .unwrap_err()
                .reason
                .contains("more than once")
        );
        assert!(
            trim("Fix the bug.", &cuts(&["Fix the bugs."]))
                .unwrap_err()
                .reason
                .contains("word for word")
        );
        assert!(trim("Fix the bug.", &cuts(&["  "])).is_err());
    }

    #[test]
    fn overlapping_cuts_and_cutting_everything_are_refused() {
        let message = "One. Two. Three.";
        assert!(
            trim(message, &cuts(&["One. Two.", "Two. Three."]))
                .unwrap_err()
                .reason
                .contains("overlap")
        );
        assert!(
            trim(message, &cuts(&["One. Two. Three."]))
                .unwrap_err()
                .reason
                .contains("leave nothing")
        );
    }

    #[test]
    fn too_many_cuts_are_refused() {
        let message = (0..=MAX_CUTS)
            .map(|i| format!("S{i}."))
            .collect::<Vec<_>>()
            .join(" ");
        let all: Vec<String> = (0..=MAX_CUTS).map(|i| format!("S{i}.")).collect();
        assert!(trim(&message, &all).unwrap_err().reason.contains("at most"));
    }

    #[test]
    fn kept_text_is_the_users_own_characters_in_order() {
        let message = "First: fix the parser!  Then, after C-9 finishes, ping me.\nAlso add tests.";
        let trimmed = trim(message, &cuts(&["Then, after C-9 finishes, ping me."])).unwrap();
        assert_eq!(trimmed, "First: fix the parser!\nAlso add tests.");
        // Every kept character appears in the original, in order.
        let mut rest = message.chars();
        for c in trimmed.chars() {
            assert!(rest.any(|o| o == c), "{c:?} not from the original in order");
        }
    }
}
