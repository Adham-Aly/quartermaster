//! Text measuring and wrapping in terminal cells.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The width of `s` in terminal cells.
pub(crate) fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The width of a character in terminal cells (0 for control characters).
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Converts a cell count to `u16`, saturating.
pub(crate) fn cells(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Word-wraps `text` to `max` cells per line. Explicit newlines are kept; a word longer than a
/// line is broken at the cell boundary. Always returns at least one (possibly empty) line.
pub(crate) fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0;
        for word in paragraph.split(' ') {
            let word_width = width(word);
            let gap = usize::from(!line.is_empty());
            if line_width + gap + word_width <= max {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                line_width += gap + word_width;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            for c in word.chars() {
                let w = char_width(c);
                if line_width + w > max && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                line.push(c);
                line_width += w;
            }
        }
        lines.push(line);
    }
    lines
}

/// The last `max` cells of `s` (for an input line wider than its box).
pub(crate) fn tail(s: &str, max: usize) -> &str {
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = char_width(c);
        if used + w > max {
            break;
        }
        used += w;
        start = i;
    }
    &s[start..]
}

/// `s` cut to at most `max` cells, ending in `…` when cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    // "the …" reads as a gap: the ellipsis follows the last word.
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// Word-wraps `text` like [`wrap`], marking each row that continues the previous one after a
/// soft wrap (`true`) rather than starting a paragraph (`false`).
pub(crate) fn wrap_rows(text: &str, max: usize) -> Vec<(String, bool)> {
    text.split('\n')
        .flat_map(|paragraph| {
            wrap(paragraph, max)
                .into_iter()
                .enumerate()
                .map(|(i, row)| (row, i > 0))
        })
        .collect()
}

/// Word-wraps runs of styled text to `max` cells per row, keeping each character's style. Words
/// break at spaces only, and a word longer than a row breaks at the cell boundary. Always returns
/// at least one (possibly empty) row; each row is a list of runs, adjacent equal styles merged.
pub(crate) fn wrap_styled<S: Copy + PartialEq>(
    runs: &[(String, S)],
    max: usize,
) -> Vec<Vec<(String, S)>> {
    let max = max.max(1);
    // Words as lists of styled characters, with the style of the space before each.
    let mut words: Vec<Word<S>> = vec![(Vec::new(), None)];
    for (text, style) in runs {
        for c in text.chars() {
            if c == ' ' {
                words.push((Vec::new(), Some(*style)));
            } else if let Some((word, _)) = words.last_mut() {
                word.push((c, *style));
            }
        }
    }
    let mut rows: Vec<Vec<(char, S)>> = Vec::new();
    let mut row: Vec<(char, S)> = Vec::new();
    let mut row_width = 0;
    for (word, space) in keep_arrows_on(words) {
        let word_width: usize = word.iter().map(|&(c, _)| char_width(c)).sum();
        let gap = usize::from(row_width > 0);
        if row_width + gap + word_width <= max {
            if gap == 1
                && let Some(style) = space
            {
                row.push((' ', style));
            }
            row_width += gap + word_width;
            row.extend(word);
            continue;
        }
        if row_width > 0 {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        for (c, s) in word {
            let w = char_width(c);
            if row_width + w > max && row_width > 0 {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push((c, s));
            row_width += w;
        }
    }
    rows.push(row);
    rows.into_iter().map(merge_runs).collect()
}

/// A word of styled characters, and the style of the space before it (none for the first).
type Word<S> = (Vec<(char, S)>, Option<S>);

/// The words with each lone arrow joined to the word after it, so a row never ends in one
/// (`qm/task-0042` then `← main`).
fn keep_arrows_on<S: Copy>(words: Vec<Word<S>>) -> Vec<Word<S>> {
    let mut out = Vec::with_capacity(words.len());
    let mut words = words.into_iter();
    while let Some((mut word, space)) = words.next() {
        if let [(arrow @ ('←' | '→'), style)] = word[..]
            && let Some((next, next_space)) = words.next()
        {
            word = vec![(arrow, style), (' ', next_space.unwrap_or(style))];
            word.extend(next);
        }
        out.push((word, space));
    }
    out
}

/// Joins adjacent characters of the same style into runs.
fn merge_runs<S: Copy + PartialEq>(chars: Vec<(char, S)>) -> Vec<(String, S)> {
    let mut out: Vec<(String, S)> = Vec::new();
    for (c, s) in chars {
        match out.last_mut() {
            Some((text, style)) if *style == s => text.push(c),
            _ => out.push((c.to_string(), s)),
        }
    }
    out
}

/// Characters, each with its style.
type Styled<S> = Vec<(char, S)>;

/// What joins the parts of a value ("high · effort Extra high").
const PART_SEPARATOR: &str = " · ";

/// Wraps runs like [`wrap_styled`], except that a value made of parts joined by " · " breaks
/// between parts: each row holds whole parts, joined as they were, and only a part longer than a
/// row is word-wrapped. A value that fits stays one row.
pub(crate) fn wrap_parts<S: Copy + PartialEq>(
    runs: &[(String, S)],
    max: usize,
) -> Vec<Vec<(String, S)>> {
    let max = max.max(1);
    let chars: Vec<(char, S)> = runs
        .iter()
        .flat_map(|(text, style)| text.chars().map(move |c| (c, *style)))
        .collect();
    let width_of = |cs: &[(char, S)]| cs.iter().map(|&(c, _)| char_width(c)).sum::<usize>();
    let separator: Vec<char> = PART_SEPARATOR.chars().collect();
    let at_separator = |i: usize| {
        chars
            .get(i..i + separator.len())
            .is_some_and(|window| window.iter().map(|&(c, _)| c).eq(separator.iter().copied()))
    };
    if width_of(&chars) <= max || !(0..chars.len()).any(at_separator) {
        return wrap_styled(runs, max);
    }

    // The parts, each with the separator (and its styles) before it; the first has none.
    let mut parts: Vec<(Styled<S>, Styled<S>)> = vec![(Vec::new(), Vec::new())];
    let mut i = 0;
    while i < chars.len() {
        if at_separator(i) {
            let joint = chars[i..i + separator.len()].to_vec();
            parts.push((joint, Vec::new()));
            i += separator.len();
        } else {
            if let Some((_, part)) = parts.last_mut() {
                part.push(chars[i]);
            }
            i += 1;
        }
    }

    let mut rows: Vec<Vec<(String, S)>> = Vec::new();
    let mut row: Vec<(char, S)> = Vec::new();
    for (joint, part) in parts {
        let part_width = width_of(&part);
        if !row.is_empty() && width_of(&row) + width_of(&joint) + part_width <= max {
            row.extend(joint);
            row.extend(part);
            continue;
        }
        if !row.is_empty() {
            rows.push(merge_runs(std::mem::take(&mut row)));
        }
        if part_width <= max {
            row = part;
        } else {
            rows.extend(wrap_styled(&merge_runs(part), max));
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(merge_runs(row));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_and_ends_cut_text_on_a_word() {
        assert_eq!(truncate("the tools", 9), "the tools");
        assert_eq!(truncate("the tools", 8), "the too…");
        // A cut that ends on a space: the ellipsis follows the word, not the gap.
        assert_eq!(
            truncate("Claude Code and the GitHub CLI", 21),
            "Claude Code and the…"
        );
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn a_row_never_ends_in_an_arrow() {
        let rows = |text: &str, max| -> Vec<String> {
            wrap_styled(&[(text.to_owned(), ())], max)
                .into_iter()
                .map(|row| row.into_iter().map(|(t, ())| t).collect())
                .collect()
        };
        assert_eq!(
            rows("fix/upload-retry-on-slow-tmp ← main", 30),
            ["fix/upload-retry-on-slow-tmp", "← main"]
        );
        assert_eq!(rows("a ← b", 30), ["a ← b"]);
        assert_eq!(rows("ends in →", 30), ["ends in →"]);
    }
}
