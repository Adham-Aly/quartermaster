//! The context strip: one to three rows attached to the top of the input box, for the agent in
//! the middle column.
//!
//! Row 1 is occupancy: how full the context is against the point where the agent's engine
//! compacts by itself, from the latest single model call (never a cumulative sum), with its age.
//! Row 2 is the running or latest turn, by the categories the engine reports. Unknown and stale
//! readings say so in words. Ages are computed here, against the wall clock: drawing one sets
//! `App::ages_shown`, and `App::tick` then redraws once a second.

use crate::app::App;
use crate::session::draw::agent_label;
use crate::text::wrap_parts;
use crate::theme;
use crate::widgets::conversation::Row;
use crate::widgets::spinner;
use jiff::Timestamp;
use qm_types::api::{
    AgentContextView, AgentRef, CompactionNote, ContextBasisView, ContextOccupancy, NoteOutcome,
    PointView, TurnTokens, tokens_short,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The most rows the strip takes.
const MAX_ROWS: usize = 3;
/// Cells in the occupancy bar.
const BAR: u64 = 10;

/// A run of text and its style.
type Run = (String, Style);
/// One part of a row: runs that stay together; parts are joined by " · ".
type Part = Vec<Run>;

/// The agent's context view, if the core has one.
pub(crate) fn view_for(app: &App, agent: AgentRef) -> Option<&AgentContextView> {
    app.snapshot.context.iter().find(|c| c.agent == agent)
}

/// The conversation, strip and input areas of `middle`. The input keeps its height first (at most
/// half the column); the strip takes its rows only while strip and input together fit in half,
/// else fewer, else none.
pub(crate) fn split(app: &App, middle: Rect, input_height: u16, agent: AgentRef) -> [Rect; 3] {
    let half = middle.height / 2;
    let input_height = input_height.min(half);
    let wanted = u16::try_from(rows(app, agent, text_width(middle)).len()).unwrap_or(0);
    // A short column keeps its room for the conversation: one row under 24, two under 34.
    let wanted = wanted.min(match middle.height {
        0..24 => 1,
        24..34 => 2,
        _ => 3,
    });
    let strip_height = wanted.min(half.saturating_sub(input_height));
    let input = Rect {
        y: middle.bottom() - input_height,
        height: input_height,
        ..middle
    };
    let strip = Rect {
        y: input.y - strip_height,
        height: strip_height,
        ..middle
    };
    let conversation = Rect {
        height: middle.height - input_height - strip_height,
        ..middle
    };
    [conversation, strip, input]
}

/// The strip's text width: in line with the input's `›`, two cells in from each side.
fn text_width(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(4))
}

/// Draws the strip in `area` (from [`split`]). Not a hit target.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, agent: AgentRef) {
    if area.height == 0 {
        return;
    }
    let rows = fit(app, agent, text_width(area), usize::from(area.height));
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| {
            Line::from(
                row.into_iter()
                    .map(|(t, s)| Span::styled(t, s))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let inside = Rect {
        x: area.x + 2,
        width: area.width.saturating_sub(4),
        ..area
    };
    frame.render_widget(Paragraph::new(lines), inside);
}

/// The strip's wrapped rows at `width`, at most three.
fn rows(app: &App, agent: AgentRef, width: usize) -> Vec<Vec<Run>> {
    fit(app, agent, width, MAX_ROWS)
}

/// The rows at `width`, dropping parts until they fit in `max` rows: the reading's age first,
/// then the turn row from its end, then the occupancy row's details, never the percentage.
fn fit(app: &App, agent: AgentRef, width: usize, max: usize) -> Vec<Vec<Run>> {
    if width == 0 || max == 0 {
        return Vec::new();
    }
    let now = Timestamp::now();
    let view = view_for(app, agent);
    let (mut first, mut droppable, mut aged) = occupancy_row(app, agent, view, now);
    let mut second = view
        .and_then(|v| v.turn.as_ref())
        .map(|t| turn_row(app, t, now))
        .unwrap_or_default();
    loop {
        let mut out = wrap_row(&first, width);
        if !second.is_empty() {
            out.extend(wrap_row(&second, width));
        }
        if out.len() <= max {
            return out;
        }
        if aged {
            // The age is the last droppable part, so it goes first.
            aged = false;
            if let Some(i) = droppable.pop() {
                first.remove(i);
            }
        } else if second.len() > 1 {
            second.pop();
        } else if !second.is_empty() {
            second.clear();
        } else if let Some(i) = droppable.pop() {
            first.remove(i);
        } else {
            out.truncate(max);
            return out;
        }
    }
}

fn wrap_row(parts: &[Part], width: usize) -> Vec<Vec<Run>> {
    let mut runs: Vec<Run> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            runs.push((" · ".to_owned(), theme::dim()));
        }
        runs.extend(part.iter().cloned());
    }
    wrap_parts(&runs, width)
}

/// Row 1, the indexes of its parts that may be dropped (popped from the end), and whether the
/// last of those is the reading's age.
fn occupancy_row(
    app: &App,
    agent: AgentRef,
    view: Option<&AgentContextView>,
    now: Timestamp,
) -> (Vec<Part>, Vec<usize>, bool) {
    let label: Run = (agent_label(agent), theme::strong());
    let Some(view) = view else {
        return (
            vec![
                vec![label],
                dim_part("context unknown"),
                dim_part("no reading since the app started"),
            ],
            vec![2],
            false,
        );
    };
    let occ = &view.occupancy;
    if let Some(compacting) = view.compacting {
        let secs = now.duration_since(compacting.since).as_secs().max(0);
        app.ages_shown.set(true);
        let mut parts = vec![vec![
            label,
            (" ".to_owned(), Style::default()),
            (spinner::glyph(app).content.into_owned(), theme::ok()),
            (format!(" compacting context… {secs}s"), theme::accent()),
        ]];
        if let Some(was) = compacting.was {
            let mut words = format!("was {}", tokens_short(was));
            if let PointView::At(point) = occ.point {
                words.push_str(" of ");
                words.push_str(&tokens_short(point));
            }
            parts.push(dim_part(&words));
        }
        return (parts, vec![1], false);
    }
    let stale = occ.stale.is_some();
    let number = if stale { theme::dim() } else { theme::text() };
    let mut parts: Vec<Part> = Vec::new();
    let mut droppable = Vec::new();
    match (occ.tokens, &occ.point) {
        (Some(tokens), PointView::At(point)) => {
            let pct = percent(tokens, *point);
            let tone = if stale { theme::dim() } else { level(pct) };
            let tilde = if occ.estimate { "~" } else { "" };
            let filled = bar_cells(pct);
            parts.push(vec![
                label,
                (" ".to_owned(), Style::default()),
                ("━".repeat(filled), tone),
                (
                    "─".repeat(usize::try_from(BAR).unwrap_or(10) - filled),
                    theme::faded(),
                ),
                (format!(" {tilde}{pct}%"), tone),
            ]);
            parts.push(vec![
                (format!("{tilde}{}", tokens_short(tokens)), number),
                (" of ".to_owned(), theme::dim()),
                (tokens_short(*point), number),
                (" before auto-compact".to_owned(), theme::dim()),
            ]);
            droppable.push(1);
        }
        (Some(tokens), point) => {
            let tilde = if occ.estimate { "~" } else { "" };
            parts.push(vec![label]);
            parts.push(vec![
                (format!("{tilde}{}", tokens_short(tokens)), number),
                (" in context".to_owned(), theme::dim()),
            ]);
            parts.extend(point_parts(point, occ.window));
            droppable.extend((2..parts.len()).rev());
        }
        (None, point) => {
            parts.push(vec![label]);
            parts.push(dim_part("context unknown"));
            if let Some(why) = &occ.unknown_why {
                parts.push(dim_part(why));
            }
            let at = parts.len();
            parts.extend(point_parts(point, occ.window));
            droppable.extend((at..parts.len()).rev());
            droppable.extend((2..at).rev());
        }
    }
    // `droppable` pops from its end: the age first, then the rest.
    droppable.reverse();
    let mut aged = false;
    if occ.tokens.is_some()
        && let Some(age) = age_words(app, occ, now)
    {
        droppable.push(parts.len());
        parts.push(dim_part(&age));
        aged = true;
    }
    if let Some(stale) = &occ.stale {
        parts.push(vec![(format!("stale: {stale}"), theme::warn())]);
    }
    (parts, droppable, aged)
}

/// The parts that say where the engine compacts, when the bar can't.
fn point_parts(point: &PointView, window: Option<u64>) -> Vec<Part> {
    let mut parts = Vec::new();
    match point {
        PointView::At(point) => {
            parts.push(dim_part(&format!(
                "auto-compact at {}",
                tokens_short(*point)
            )));
        }
        PointView::Off => parts.push(dim_part("auto-compact off")),
        PointView::Unknown(why) => {
            parts.push(dim_part("auto-compact point unknown"));
            parts.push(dim_part(why));
        }
        PointView::NotReported => parts.push(dim_part("auto-compact point unknown")),
    }
    if !matches!(point, PointView::At(_))
        && let Some(window) = window
    {
        parts.push(dim_part(&format!("{} window", tokens_short(window))));
    }
    parts
}

/// `12s ago`, `at session start, 4m ago`, `after compaction, 8s ago`.
fn age_words(app: &App, occ: &ContextOccupancy, now: Timestamp) -> Option<String> {
    let at = occ.observed_at?;
    let ago = format!("{} ago", age(app, at, now));
    Some(match occ.basis {
        ContextBasisView::SessionStart => format!("at session start, {ago}"),
        ContextBasisView::AfterCompaction => format!("after compaction, {ago}"),
        ContextBasisView::LastCall | ContextBasisView::None => ago,
    })
}

/// Row 2: the running turn so far, or the latest finished one.
fn turn_row(app: &App, turn: &TurnTokens, now: Timestamp) -> Vec<Part> {
    let mut parts = vec![dim_part(if turn.running {
        "this turn so far"
    } else {
        "last turn"
    })];
    let mut count = |label: &str, n: u64| {
        parts.push(vec![
            (format!("{label} "), theme::dim()),
            (tokens_short(n), theme::text()),
        ]);
    };
    count("in", turn.input);
    if let Some(n) = turn.cache_read {
        count("cache read", n);
    }
    if let Some(n) = turn.cache_write {
        count("cache write", n);
    }
    count("out", turn.output);
    if let Some(n) = turn.thinking {
        count("thinking", n);
    }
    if let Some(cost) = &turn.cost {
        parts.push(vec![(cost.clone(), theme::text())]);
    }
    let calls = match turn.model_calls {
        1 => "1 call".to_owned(),
        n => format!("{n} calls"),
    };
    parts.push(vec![(calls, theme::text())]);
    if !turn.running
        && let Some(at) = turn.at
    {
        parts.push(dim_part(&format!("{} ago", age(app, at, now))));
    }
    parts
}

/// The task card's `context` value: the strip's first row without the label or bar.
pub(crate) fn card_runs(app: &App, view: Option<&AgentContextView>) -> Vec<Run> {
    let now = Timestamp::now();
    let mut parts: Vec<Part> = Vec::new();
    match view {
        None => {
            parts.push(dim_part("unknown"));
            parts.push(dim_part("no reading since the app started"));
        }
        Some(view) if view.compacting.is_some() => {
            let since = view.compacting.map_or(now, |c| c.since);
            let secs = now.duration_since(since).as_secs().max(0);
            app.ages_shown.set(true);
            parts.push(vec![(format!("compacting… {secs}s"), theme::accent())]);
        }
        Some(view) => {
            let occ = &view.occupancy;
            let tilde = if occ.estimate { "~" } else { "" };
            match (occ.tokens, &occ.point) {
                (Some(tokens), PointView::At(point)) => {
                    let pct = percent(tokens, *point);
                    let tone = if occ.stale.is_some() {
                        theme::dim()
                    } else {
                        level(pct)
                    };
                    parts.push(vec![(format!("{tilde}{pct}%"), tone)]);
                    parts.push(vec![
                        (format!("{tilde}{}", tokens_short(tokens)), theme::text()),
                        (" of ".to_owned(), theme::dim()),
                        (tokens_short(*point), theme::text()),
                    ]);
                }
                (Some(tokens), point) => {
                    parts.push(vec![(
                        format!("{tilde}{}", tokens_short(tokens)),
                        theme::text(),
                    )]);
                    parts.extend(point_parts(point, None));
                }
                (None, _) => {
                    parts.push(dim_part("unknown"));
                    if let Some(why) = &occ.unknown_why {
                        parts.push(dim_part(why));
                    }
                }
            }
            if occ.tokens.is_some()
                && let Some(age) = age_words(app, occ, now)
            {
                parts.push(dim_part(&age));
            }
            if let Some(stale) = &occ.stale {
                parts.push(vec![(format!("stale: {stale}"), theme::warn())]);
            }
        }
    }
    let mut runs = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            runs.push((" · ".to_owned(), theme::dim()));
        }
        runs.extend(part);
    }
    runs
}

fn dim_part(words: &str) -> Part {
    vec![(words.to_owned(), theme::dim())]
}

/// `tokens` as a whole percentage of `point`, rounded.
fn percent(tokens: u64, point: u64) -> u64 {
    if point == 0 {
        return 100;
    }
    (tokens.saturating_mul(100) + point / 2) / point
}

/// Filled bar cells: a tenth each, at least one above zero, all ten over 100%.
fn bar_cells(pct: u64) -> usize {
    let filled = ((pct + 5) / 10).min(BAR);
    let filled = if pct > 0 { filled.max(1) } else { 0 };
    usize::try_from(filled).unwrap_or(0)
}

/// The tone for a level: fine below 70%, a warning to 89%, then an error.
fn level(pct: u64) -> Style {
    match pct {
        0..70 => theme::ok(),
        70..90 => theme::warn(),
        _ => theme::error(),
    }
}

/// `12s`, `4m`, `2h`, `3d`; drawing one keeps the once-a-second redraw going.
pub(crate) fn age(app: &App, at: Timestamp, now: Timestamp) -> String {
    app.ages_shown.set(true);
    let secs = now.duration_since(at).as_secs().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The compaction notes a chat shows, oldest first: every failure, and finished automatic ones
/// (one you asked for with `/compact` already says so in its own turn).
pub(crate) fn chat_notes(app: &App, agent: AgentRef) -> Vec<&CompactionNote> {
    view_for(app, agent).map_or_else(Vec::new, |view| {
        view.notes
            .iter()
            .filter(|n| n.automatic || matches!(n.outcome, NoteOutcome::Failed(_)))
            .collect()
    })
}

/// A note's rows at `width`: `✓ Context compacted automatically · 182k → ~21k · took 4s`, or a
/// failure in the error colour.
pub(crate) fn note_rows(note: &CompactionNote, width: u16) -> Vec<Row> {
    let how = if note.automatic { " automatically" } else { "" };
    let mut runs: Vec<Run> = match &note.outcome {
        NoteOutcome::Done => vec![
            ("✓ ".to_owned(), theme::ok()),
            (format!("Context compacted{how}"), theme::dim()),
        ],
        NoteOutcome::Failed(why) => vec![
            ("✗ ".to_owned(), theme::error()),
            (format!("Context compaction failed: {why}"), theme::error()),
        ],
    };
    if let (Some(before), Some(after)) = (note.before, note.after) {
        let tilde = if note.after_estimate { "~" } else { "" };
        runs.push((
            format!(
                " · {} → {tilde}{}",
                tokens_short(before),
                tokens_short(after)
            ),
            theme::dim(),
        ));
    }
    if let Some(ms) = note.took_ms {
        runs.push((format!(" · took {}s", ms.div_ceil(1000)), theme::dim()));
    }
    wrap_parts(&runs, usize::from(width))
        .into_iter()
        .map(|row| {
            Row::text(
                Line::from(
                    row.into_iter()
                        .map(|(t, s)| Span::styled(t, s))
                        .collect::<Vec<_>>(),
                ),
                false,
            )
        })
        .collect()
}

/// The busy row while the agent's context is being compacted: `⣯ Compacting context… 12s`.
pub(crate) fn compacting_row(app: &App, agent: AgentRef) -> Option<Row> {
    let compacting = view_for(app, agent)?.compacting?;
    let secs = Timestamp::now()
        .duration_since(compacting.since)
        .as_secs()
        .max(0);
    app.ages_shown.set(true);
    Some(Row::plain(spinner::line(
        app,
        &format!("Compacting context… {secs}s"),
    )))
}

/// Every note in `notes` from before `at`, as rows each after a blank row; `notes` keeps the rest.
pub(crate) fn notes_before(
    notes: &mut &[&CompactionNote],
    at: Option<Timestamp>,
    width: u16,
) -> Vec<Row> {
    let mut out = Vec::new();
    while let Some((note, rest)) = notes.split_first() {
        if at.is_some_and(|at| note.at > at) {
            break;
        }
        out.push(Row::plain(Line::default()));
        out.extend(note_rows(note, width));
        *notes = rest;
    }
    out
}
