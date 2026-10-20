//! The setup check's rows, cell by cell: the check itself ("Setup check ·
//! studio"), and the compact "Setup check" box beside the terminal panes.
//!
//! Each builder is a pure function returning [`Row`]s, a line and the clickable runs on it.

use super::SetupHit;
use crate::text::{cells, truncate, width, wrap};
use crate::theme;
use crate::widgets::button;
use qm_types::api::{SetupActionKind, SetupItem, SetupMark, SetupTool, SetupView, Styled};
use ratatui::text::{Line, Span};

/// Cells the lines under an item's name are indented by.
pub(super) const INDENT: usize = 4;
/// Cells between the widest name and the state column in the list of what needs you (without
/// Homebrew: `GitHub CLI (gh)` + 3 → `not installed`; its steps align the same way).
const NAME_GAP: usize = 3;
/// The name column of the full check, where the tools that are fine are listed too (every state
/// starts 27 cells after the mark and its space, `git 2.50.1` and `Claude Code
/// 2.1.303` alike). A wider name pushes it out by [`NAME_GAP`].
const FULL_NAME_COLUMN: usize = 27;

/// One drawn row and what can be clicked on it, by offset into the row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Row {
    pub(super) line: Line<'static>,
    /// `(x offset, width, hit)`.
    pub(super) hits: Vec<(u16, u16, SetupHit)>,
}

impl Row {
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            hits: Vec::new(),
        }
    }

    /// The cells the row's spans take so far.
    fn len(&self) -> u16 {
        cells(self.line.width())
    }

    fn push(&mut self, span: Span<'static>) {
        self.line.spans.push(span);
    }

    /// Adds a button, highlighted when `lit`.
    fn button(&mut self, label: &str, lit: bool, hit: SetupHit) {
        let x = self.len();
        self.hits.push((x, button::width_of(label), hit));
        self.push(button::span(label, lit));
    }
}

/// An item's mark: `✓` ok, `✗` missing, `!` needs you.
pub(super) fn mark(mark: SetupMark) -> Span<'static> {
    match mark {
        SetupMark::Ok => Span::styled("✓", theme::ok()),
        SetupMark::Missing => Span::styled("✗", theme::error()),
        _ => Span::styled("!", theme::warn()),
    }
}

fn styled(runs: &[Styled]) -> Vec<Span<'static>> {
    runs.iter()
        .map(|s| Span::styled(s.text.clone(), theme::tone(s.tone)))
        .collect()
}

fn dim_rows(text: &str, max: usize, indent: usize) -> Vec<Row> {
    wrap(text, max.saturating_sub(indent).max(1))
        .into_iter()
        .map(|l| {
            Row::plain(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(l, theme::dim()),
            ]))
        })
        .collect()
}

/// Whether an action is what `⏎` does for its item: the item's first `Run`.
pub(super) fn default_action(item: &SetupItem) -> Option<usize> {
    item.actions
        .iter()
        .position(|a| matches!(a.kind, SetupActionKind::Run { .. }))
}

/// The check's rows at `width` cells. `current` is the item the keys act on; its `⏎` button is
/// lit, as is any button under the pointer.
pub(super) fn check_rows(
    view: &SetupView,
    width_cells: u16,
    current: Option<usize>,
    hover: Option<SetupHit>,
) -> Vec<Row> {
    let max = usize::from(width_cells);
    // Without Homebrew the check is the list of what needs you: the tools that
    // are fine aren't listed (they are in the full check once nothing needs copying by hand).
    let by_hand = view.items.iter().any(|i| !i.steps.is_empty());
    let shown = |item: &SetupItem| !(by_hand && item.mark == SetupMark::Ok);
    let widest = view
        .items
        .iter()
        .filter(|i| shown(i))
        .map(|i| width(&i.name))
        .max()
        .unwrap_or(0);
    let name_column = if by_hand {
        widest + NAME_GAP
    } else {
        FULL_NAME_COLUMN.max(widest + NAME_GAP)
    };
    let mut rows = Vec::new();
    let mut previous_had_more = false;
    for (index, item) in view.items.iter().enumerate() {
        if !shown(item) {
            continue;
        }
        let more = item.explanation.is_some()
            || !item.steps.is_empty()
            || item.footnote.is_some()
            || !item.actions.is_empty();
        if previous_had_more && item.mark != SetupMark::Ok {
            rows.push(Row::default());
        }
        previous_had_more = more;
        rows.push(item_row(item, name_column, max));
        if let Some(explanation) = &item.explanation {
            // The explanation starts at the left edge, under the mark.
            rows.extend(dim_rows(explanation, max, 0));
        }
        rows.extend(step_rows(item, index, max, hover));
        if let Some(footnote) = &item.footnote {
            rows.extend(dim_rows(footnote, max, INDENT));
        }
        if !item.actions.is_empty() {
            rows.push(action_row(item, index, current == Some(index), hover));
        }
    }
    if !view.checked {
        rows.push(Row::plain(Line::styled("Checking this Mac…", theme::dim())));
    }
    rows
}

/// `✓ git 2.50.1                 Xcode Command Line Tools`.
fn item_row(item: &SetupItem, name_column: usize, max: usize) -> Row {
    let name_style = if item.mark == SetupMark::Ok {
        theme::text()
    } else {
        theme::strong()
    };
    let mut spans = vec![
        mark(item.mark),
        Span::raw(" "),
        Span::styled(item.name.clone(), name_style),
        Span::raw(" ".repeat(name_column.saturating_sub(width(&item.name)))),
    ];
    spans.extend(styled(&item.state));
    let mut line = Line::from(spans);
    if line.width() > max {
        // Cut the state runs to fit, keeping their colours.
        let mut room = max;
        for span in &mut line.spans {
            let w = width(&span.content);
            if w <= room {
                room -= w;
            } else {
                span.content = truncate(&span.content, room).into();
                room = 0;
            }
        }
    }
    Row::plain(line)
}

/// `    1. Install Homebrew   /bin/bash -c "…"  Copy`.
fn step_rows(item: &SetupItem, index: usize, max: usize, hover: Option<SetupHit>) -> Vec<Row> {
    let label_column = item
        .steps
        .iter()
        .map(|s| width(&s.label))
        .max()
        .unwrap_or(0)
        + NAME_GAP;
    item.steps
        .iter()
        .enumerate()
        .map(|(step, s)| {
            let mut row = Row::plain(Line::from(vec![
                Span::raw(" ".repeat(INDENT)),
                Span::raw(s.label.clone()),
                Span::raw(" ".repeat(label_column.saturating_sub(width(&s.label)))),
            ]));
            let copy = "Copy";
            let room = max
                .saturating_sub(usize::from(row.len()))
                .saturating_sub(2 + usize::from(button::width_of(copy)));
            row.push(Span::styled(truncate(&s.command, room), theme::dim()));
            row.push(Span::raw("  "));
            let hit = SetupHit::CopyStep { item: index, step };
            row.button(copy, hover == Some(hit), hit);
            row
        })
        .collect()
}

/// `    Install with Homebrew  Copy the command  Not now` and the dim hint after them.
fn action_row(item: &SetupItem, index: usize, current: bool, hover: Option<SetupHit>) -> Row {
    let lit_default = current.then(|| default_action(item)).flatten();
    let mut row = Row::plain(Line::from(Span::raw(" ".repeat(INDENT))));
    for (action, a) in item.actions.iter().enumerate() {
        if action > 0 {
            row.push(Span::raw(" "));
        }
        let hit = SetupHit::Action {
            item: index,
            action,
        };
        row.button(
            &a.label,
            hover == Some(hit) || lit_default == Some(action),
            hit,
        );
    }
    if let Some(hint) = &item.hint {
        row.push(Span::raw(" "));
        row.push(Span::styled(hint.clone(), theme::dim()));
    }
    row
}

/// The compact box beside the panes: each tool the panes worked on, then Done.
pub(super) fn summary_rows(
    view: &SetupView,
    tools: &[SetupTool],
    hover: Option<SetupHit>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for tool in tools {
        let Some(item) = view.items.iter().find(|i| i.tool == *tool) else {
            continue;
        };
        rows.push(Row::plain(Line::from(vec![
            mark(item.mark),
            Span::raw(" "),
            Span::raw(item.summary.clone()),
        ])));
        for detail in &item.details {
            rows.push(Row::plain(Line::from(vec![
                Span::raw("  "),
                Span::styled(detail.clone(), theme::dim()),
            ])));
        }
    }
    let mut done = Row::plain(Line::from(Span::raw("  ")));
    done.button("Done", hover == Some(SetupHit::Done), SetupHit::Done);
    rows.push(done);
    rows
}

/// A finished pane's last lines: what came of it, and the next step's button (lit: it's what
/// `⏎` does).
pub(super) fn result_rows(
    result: Option<Line<'static>>,
    next: Option<&str>,
    run: usize,
) -> Vec<Row> {
    let mut rows = vec![Row::default()];
    if let Some(result) = result {
        rows.push(Row::plain(result));
    }
    if let Some(label) = next {
        let mut row = Row::plain(Line::from(Span::raw("  ")));
        let hit = SetupHit::Next { run };
        row.button(label, true, hit);
        rows.push(row);
    }
    rows
}
