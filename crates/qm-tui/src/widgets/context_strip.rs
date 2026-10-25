//! The context strip: one line in the bottom border of the middle column's box, for the agent
//! shown there (`━━━━━─── 61% context · $0.04 last turn · 3 calls`), and the fuller lines the
//! task pane's Usage section shows.
//!
//! Occupancy is how full the context is against the point where the agent's engine compacts by
//! itself, from the latest single model call (never a cumulative sum), with its age. The turn is
//! the running or latest one, by the categories the engine reports. Unknown and stale readings
//! say so in words. Ages are computed here, against the wall clock: drawing one sets
//! `App::ages_shown`, and `App::tick` then redraws once a second.

use crate::app::App;
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
use ratatui::widgets::{Clear, Paragraph};

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

/// The conversation and input areas of `middle`: the input keeps its height (at most half the
/// column), the conversation's box takes the rest.
pub(crate) fn split(middle: Rect, input_height: u16) -> [Rect; 2] {
    let input_height = input_height.min(middle.height / 2);
    let input = Rect {
        y: middle.bottom() - input_height,
        height: input_height,
        ..middle
    };
    let conversation = Rect {
        height: middle.height - input_height,
        ..middle
    };
    [conversation, input]
}

/// The strip in the bottom border of the box (`area` is that border row), right-aligned with a
/// `─` before the corner, leaving `left` cells from the left edge free. Not a hit target.
pub(crate) fn draw_in_border(
    frame: &mut Frame<'_>,
    area: Rect,
    left: u16,
    app: &App,
    agent: AgentRef,
) {
    // ` runs ` plus the `─╯` after it.
    let room = usize::from(area.width.saturating_sub(left + 4));
    let runs = border_runs(app, agent, room);
    if runs.is_empty() {
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(runs.into_iter().map(|(t, s)| Span::styled(t, s)));
    spans.push(Span::raw(" "));
    let line = Line::from(spans);
    let w = u16::try_from(line.width()).unwrap_or(u16::MAX);
    let x = area.right().saturating_sub(w + 2);
    let rect = Rect::new(x, area.y, w, 1);
    // Plain-text runs must not take the border's colour from the cells under them.
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(line), rect);
}

/// The strip's one line at no more than `max` cells: `━━━━━─── 61% context · $0.04 last turn ·
/// 3 calls`, dropping from its end (the calls, the turn, the word) to fit; the details are in
/// the task pane's Usage section.
fn border_runs(app: &App, agent: AgentRef, max: usize) -> Vec<Run> {
    let now = Timestamp::now();
    let view = view_for(app, agent);
    let mut lead: Vec<Run> = Vec::new();
    let mut word: Vec<Run> = Vec::new();
    match view {
        None => lead.push(("context unknown".to_owned(), theme::dim())),
        Some(view) if view.compacting.is_some() => {
            let since = view.compacting.map_or(now, |c| c.since);
            let secs = now.duration_since(since).as_secs().max(0);
            app.ages_shown.set(true);
            lead.push((
                spinner::glyph(app, spinner::Spinner::Asterism)
                    .content
                    .into_owned(),
                theme::ok(),
            ));
            lead.push((format!(" compacting context… {secs}s"), theme::accent()));
        }
        Some(view) => {
            let occ = &view.occupancy;
            let tilde = if occ.estimate { "~" } else { "" };
            match (occ.tokens, &occ.point) {
                (Some(tokens), PointView::At(point)) => {
                    let pct = percent(tokens, *point);
                    let stale = occ.stale.is_some();
                    let tone = if stale { theme::dim() } else { level(pct) };
                    let fill = if stale || pct >= 70 {
                        tone
                    } else {
                        theme::accent()
                    };
                    let filled = bar_cells(pct);
                    let bar = usize::try_from(BAR).unwrap_or(10);
                    lead.push(("━".repeat(filled), fill));
                    lead.push(("━".repeat(bar - filled), theme::rule()));
                    lead.push((format!(" {tilde}{pct}%"), tone));
                    word.push((" context".to_owned(), theme::dim()));
                    if stale {
                        word.push((" · stale".to_owned(), theme::warn()));
                    }
                }
                (Some(tokens), _) => {
                    lead.push((format!("{tilde}{}", tokens_short(tokens)), theme::text()));
                    word.push((" in context".to_owned(), theme::dim()));
                }
                (None, _) => lead.push(("context unknown".to_owned(), theme::dim())),
            }
        }
    }
    let mut turn: Vec<Run> = Vec::new();
    let mut calls: Vec<Run> = Vec::new();
    if let Some(t) = view.and_then(|v| v.turn.as_ref()) {
        let when = if t.running { "this turn" } else { "last turn" };
        if let Some(cost) = &t.cost {
            turn.push((" · ".to_owned(), theme::dim()));
            turn.push((cost.clone(), theme::text()));
            turn.push((format!(" {when}"), theme::dim()));
            let n = match t.model_calls {
                1 => "1 call".to_owned(),
                n => format!("{n} calls"),
            };
            calls.push((format!(" · {n}"), theme::dim()));
        } else {
            let n = match t.model_calls {
                1 => "1 call".to_owned(),
                n => format!("{n} calls"),
            };
            turn.push((" · ".to_owned(), theme::dim()));
            turn.push((n, theme::text()));
            turn.push((format!(" {when}"), theme::dim()));
        }
    }
    let len = |runs: &[Run]| {
        runs.iter()
            .map(|(t, _)| crate::text::width(t))
            .sum::<usize>()
    };
    for take in [
        vec![&lead, &word, &turn, &calls],
        vec![&lead, &word, &turn],
        vec![&lead, &word],
        vec![&lead],
    ] {
        let runs: Vec<Run> = take.into_iter().flatten().cloned().collect();
        if len(&runs) <= max {
            return runs;
        }
    }
    Vec::new()
}

/// The task pane's turn lines at `width`: the running turn so far, or the latest one, by the
/// categories the engine reports.
pub(crate) fn turn_lines(app: &App, agent: AgentRef, width: usize) -> Vec<Vec<Run>> {
    let now = Timestamp::now();
    view_for(app, agent)
        .and_then(|v| v.turn.as_ref())
        .map(|t| wrap_row(&turn_row(app, t, now), width))
        .unwrap_or_default()
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
                    let number = if occ.stale.is_some() {
                        theme::dim()
                    } else {
                        theme::text()
                    };
                    parts.push(vec![(format!("{tilde}{pct}%"), tone)]);
                    parts.push(vec![
                        (format!("{tilde}{}", tokens_short(tokens)), number),
                        (" of ".to_owned(), theme::dim()),
                        (tokens_short(*point), number),
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

/// The task pane's context lines at `width` cells: a bar across the pane with the percentage,
/// then `106k of 167k before auto-compact · 2m ago`. Without a point to measure against (or while
/// compacting), the card's words alone.
pub(crate) fn usage_lines(
    app: &App,
    view: Option<&AgentContextView>,
    width: usize,
) -> Vec<Vec<Run>> {
    let now = Timestamp::now();
    let Some(v) = view.filter(|v| v.compacting.is_none()) else {
        return vec![card_runs(app, view)];
    };
    let occ = &v.occupancy;
    let (Some(tokens), PointView::At(point)) = (occ.tokens, &occ.point) else {
        return vec![card_runs(app, view)];
    };
    let pct = percent(tokens, *point);
    let stale = occ.stale.is_some();
    let tone = if stale { theme::dim() } else { level(pct) };
    let fill = if stale || pct >= 70 {
        tone
    } else {
        theme::accent()
    };
    let tilde = if occ.estimate { "~" } else { "" };
    let label = format!(" {tilde}{pct}%");
    let bar = width.saturating_sub(crate::text::width(&label)).max(1);
    let filled = if pct == 0 {
        0
    } else {
        let pct = usize::try_from(pct.min(100)).unwrap_or(100);
        ((bar * pct + 50) / 100).clamp(1, bar)
    };
    let number = if stale { theme::dim() } else { theme::text() };
    let mut words = vec![
        (format!("{tilde}{}", tokens_short(tokens)), number),
        (" of ".to_owned(), theme::dim()),
        (tokens_short(*point), number),
        (" before auto-compact".to_owned(), theme::dim()),
    ];
    if let Some(age) = age_words(app, occ, now) {
        words.push((format!(" · {age}"), theme::dim()));
    }
    if let Some(stale) = &occ.stale {
        words.push((" · ".to_owned(), theme::dim()));
        words.push((format!("stale: {stale}"), theme::warn()));
    }
    vec![
        vec![
            ("━".repeat(filled), fill),
            ("━".repeat(bar - filled), theme::rule()),
            (label, tone),
        ],
        words,
    ]
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

/// `12s`, `4m`, `2h`, `3d`; drawing a seconds count keeps the once-a-second redraw going.
pub(crate) fn age(app: &App, at: Timestamp, now: Timestamp) -> String {
    let secs = now.duration_since(at).as_secs().max(0);
    // Only seconds change every second; a minute count is redrawn with whatever comes next.
    if secs < 60 {
        app.ages_shown.set(true);
    }
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

/// The busy row while the agent's context is being compacted: `⁂ Compacting context… 12s`.
pub(crate) fn compacting_row(app: &App, agent: AgentRef) -> Option<Row> {
    let compacting = view_for(app, agent)?.compacting?;
    let secs = Timestamp::now()
        .duration_since(compacting.since)
        .as_secs()
        .max(0);
    app.ages_shown.set(true);
    Some(Row::plain(spinner::line(
        app,
        spinner::Spinner::Asterism,
        &format!("Compacting context… {secs}s"),
    )))
}

/// Every note in `notes` from before `at`, as rows each after a blank row (none at the very
/// `first` row of a chat); `notes` keeps the rest.
pub(crate) fn notes_before(
    notes: &mut &[&CompactionNote],
    at: Option<Timestamp>,
    width: u16,
    first: bool,
) -> Vec<Row> {
    let mut out = Vec::new();
    while let Some((note, rest)) = notes.split_first() {
        if at.is_some_and(|at| note.at > at) {
            break;
        }
        if !(first && out.is_empty()) {
            out.push(Row::plain(Line::default()));
        }
        out.extend(note_rows(note, width));
        *notes = rest;
    }
    out
}
