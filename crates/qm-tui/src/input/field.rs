//! The prompt field's layout: your message soft-wrapped into rows, the middle of a very long
//! message folded away, and which byte of the text a screen cell stands for.
//!
//! - **Wrap.** Each line of the text wraps at the field's width, after the last space that fits,
//!   or mid-word when a word is wider than the field. A space that doesn't fit hangs past the
//!   edge; the cursor after a full row sits in the field's right padding.
//! - **Grow, then scroll.** The field shows [`MIN_ROWS`] to [`MAX_ROWS`] rows; past that it
//!   scrolls to keep the cursor in view.
//! - **Fold.** A message of more than [`FOLD_ABOVE`] rows shows its first [`KEEP_HEAD`] rows,
//!   `...[truncated N lines]...`, and its last [`KEEP_TAIL`] rows, plus [`AROUND_CURSOR`] rows
//!   either side of the cursor wherever it is, so you always see what you are editing. Only the
//!   display folds: the whole text stays in the field and is sent.
//!
//! The functions are pure, so they can be checked without a terminal.

use crate::text;
use ratatui::layout::{Position, Rect};

/// The field never shows fewer rows than this.
pub(crate) const MIN_ROWS: usize = 2;
/// The field grows with your message up to this many rows, then scrolls.
pub(crate) const MAX_ROWS: usize = 10;
/// A message of more rows than this shows folded.
pub(crate) const FOLD_ABOVE: usize = 30;
/// Rows a folded message keeps from its start.
pub(crate) const KEEP_HEAD: usize = 4;
/// Rows a folded message keeps from its end.
pub(crate) const KEEP_TAIL: usize = 5;
/// Rows a folded message keeps either side of the cursor's row.
pub(crate) const AROUND_CURSOR: usize = 2;

/// One soft-wrapped row: the bytes `start..end` of the text, never holding a `\n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// One row as the field shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// Row `i` of the wrapped text.
    Row(usize),
    /// The rows `first..first + hidden`, folded into `...[truncated N lines]...`.
    Fold { first: usize, hidden: usize },
}

/// What the last frame drew of the field, for the mouse and the `↑` `↓` keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Drawn {
    /// The text's cells: right of the `› ` lead, one row per shown row in view.
    pub(crate) area: Rect,
    /// Cells a row wraps at.
    pub(crate) room: usize,
    /// The text's length when drawn: the rows describe that text only.
    pub(crate) len: usize,
    pub(crate) rows: Vec<Row>,
    /// Every shown row, in view or not.
    pub(crate) shown: Vec<Shown>,
    /// The first shown row in view.
    pub(crate) scroll: usize,
}

impl Drawn {
    /// The byte of `s` the cell at `at` stands for. Above or below the field it is the shown row
    /// just out of view, so a drag past the edge scrolls a row at a time. `None` when the text
    /// changed since the frame.
    pub(crate) fn offset_at(&self, s: &str, at: Position) -> Option<usize> {
        if s.len() != self.len || self.shown.is_empty() {
            return None;
        }
        let last = self.shown.len() - 1;
        let index = if at.y < self.area.y {
            self.scroll.saturating_sub(1)
        } else if at.y >= self.area.bottom() {
            self.scroll + usize::from(self.area.height)
        } else {
            self.scroll + usize::from(at.y - self.area.y)
        };
        let column = usize::from(at.x.saturating_sub(self.area.x));
        Some(match self.shown[index.min(last)] {
            Shown::Row(i) => offset_at(s, &self.rows, i, column),
            Shown::Fold { first, .. } => self.rows[first].start,
        })
    }
}

/// The fold row's text for `hidden` rows.
pub(crate) fn fold_label(hidden: usize) -> String {
    format!("...[truncated {hidden} lines]...")
}

fn char_width(c: char) -> usize {
    text::width(c.encode_utf8(&mut [0; 4]))
}

/// `s` wrapped to `room` cells a row.
pub(crate) fn wrap(s: &str, room: usize) -> Vec<Row> {
    let room = room.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    for line in s.split('\n') {
        wrap_line(line, line_start, room, &mut rows);
        line_start += line.len() + 1;
    }
    rows
}

/// Appends the rows of one line, which starts at byte `offset` of the text. Spaces never start a
/// row: one that doesn't fit hangs past the edge, where it shows as nothing.
fn wrap_line(line: &str, offset: usize, room: usize, rows: &mut Vec<Row>) {
    let mut start = 0;
    let mut used = 0;
    // Just after the last space on this row, and the row's width up to there.
    let mut after_space: Option<(usize, usize)> = None;
    let mut push = |from: usize, to: usize| {
        rows.push(Row {
            start: offset + from,
            end: offset + to,
        });
    };
    for (i, c) in line.char_indices() {
        let w = char_width(c);
        if c != ' ' && used + w > room && i > start {
            if let Some((at, width)) = after_space.take().filter(|&(at, _)| at > start) {
                push(start, at);
                start = at;
                used -= width;
            }
            // A word wider than the row, or a wide character that still doesn't fit.
            if used + w > room && i > start {
                push(start, i);
                start = i;
                used = 0;
            }
        }
        used += w;
        if c == ' ' {
            after_space = Some((i + 1, used));
        }
    }
    push(start, line.len());
}

/// Whether row `i` runs on into the next one (a soft wrap) rather than ending its line.
fn runs_on(rows: &[Row], i: usize) -> bool {
    rows.get(i + 1)
        .is_some_and(|next| next.start == rows[i].end && rows[i].end > rows[i].start)
}

/// The row the cursor at byte `at` shows on: where it falls inside a row, or at the end of a row
/// that ends its line.
pub(crate) fn row_of(rows: &[Row], at: usize) -> usize {
    rows.iter()
        .enumerate()
        .find(|&(i, row)| row.start <= at && (at < row.end || (at == row.end && !runs_on(rows, i))))
        .map_or(rows.len().saturating_sub(1), |(i, _)| i)
}

/// The cursor's column on its row, in cells.
pub(crate) fn column_of(s: &str, rows: &[Row], at: usize) -> usize {
    let row = rows[row_of(rows, at)];
    text::width(&s[row.start..at.clamp(row.start, row.end)])
}

/// The byte on row `i` at `column` cells in: the character covering that cell, or the row's end.
/// A row that runs on stops before its last character, so the cursor stays on that row.
pub(crate) fn offset_at(s: &str, rows: &[Row], i: usize, column: usize) -> usize {
    let row = rows[i];
    let mut used = 0;
    for (j, c) in s[row.start..row.end].char_indices() {
        let w = char_width(c);
        if column < used + w.max(1) {
            return row.start + j;
        }
        used += w;
    }
    if runs_on(rows, i) {
        s[row.start..row.end]
            .char_indices()
            .next_back()
            .map_or(row.end, |(j, _)| row.start + j)
    } else {
        row.end
    }
}

/// The cursor one row up at the same column, or `at` itself on the first row.
pub(crate) fn row_up(s: &str, rows: &[Row], at: usize) -> usize {
    let i = row_of(rows, at);
    if i == 0 {
        return at;
    }
    offset_at(s, rows, i - 1, column_of(s, rows, at))
}

/// The cursor one row down at the same column, or `at` itself on the last row.
pub(crate) fn row_down(s: &str, rows: &[Row], at: usize) -> usize {
    let i = row_of(rows, at);
    if i + 1 >= rows.len() {
        return at;
    }
    offset_at(s, rows, i + 1, column_of(s, rows, at))
}

/// The rows the field shows for `count` wrapped rows with the cursor on row `cursor`: all of
/// them up to [`FOLD_ABOVE`], else the kept rows with each longer run between them folded.
pub(crate) fn fold(count: usize, cursor: usize) -> Vec<Shown> {
    if count <= FOLD_ABOVE {
        return (0..count).map(Shown::Row).collect();
    }
    let kept =
        |i: usize| i < KEEP_HEAD || i + KEEP_TAIL >= count || i.abs_diff(cursor) <= AROUND_CURSOR;
    let mut shown = Vec::new();
    let mut i = 0;
    while i < count {
        if kept(i) {
            shown.push(Shown::Row(i));
            i += 1;
            continue;
        }
        let first = i;
        while i < count && !kept(i) {
            i += 1;
        }
        // Folding a single row would hide nothing: the fold takes its place.
        if i - first == 1 {
            shown.push(Shown::Row(first));
        } else {
            shown.push(Shown::Fold {
                first,
                hidden: i - first,
            });
        }
    }
    shown
}

/// The field's text rows for `shown` rows: between [`MIN_ROWS`] and [`MAX_ROWS`].
pub(crate) fn visible_rows(shown: usize) -> usize {
    shown.clamp(MIN_ROWS, MAX_ROWS)
}

/// The first shown row to draw so that row `cursor` of `shown` is in a window of `visible`
/// rows, moving as little as possible from `scroll`.
pub(crate) fn scroll_to(scroll: usize, cursor: usize, shown: usize, visible: usize) -> usize {
    let max = shown.saturating_sub(visible);
    let mut first = scroll.min(max);
    if cursor < first {
        first = cursor;
    } else if cursor >= first + visible {
        first = cursor + 1 - visible;
    }
    first.min(max)
}
