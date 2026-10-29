//! The crew list: the Quartermaster, then the tasks in three groups (what is moving, what waits,
//! what is done), then the secondmates.
//!
//! ```text
//! ◆ Quartermaster          ready
//! Working · 2
//! ⎈ Token refresh race
//!   api-server · fix · 12m
//! ● Rate limit middleware
//!   web-app · via web-app
//!   ● build                     3m
//!   ○ review                 waits
//! Done · 1
//! ✓ Find what fills Downloads
//!   Downloads · research · reported
//! Secondmates · 1
//! ○ web-app                idle
//! ```
//!
//! Each task is one entry under its name (two or three words the Quartermaster gave it, else made
//! from its title), wrapped rather than cut, with a dim line saying where it works, what kind
//! of work and how it stands. A task with more than one crewmate lists them by kind under it, each
//! with its status. A busy crewmate's glyph is its own busy indicator (`spinner::for_agent`); a
//! flag (`⚑`) says it waits on you. A task whose every crewmate waits on you for something that is
//! in Needs you shows only there, so nothing is listed twice.
//!
//! When the column is too short for every entry, each task takes one line (`compact`).

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, heading_style, section_block};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width, wrap};
use crate::theme;
use crate::widgets::spinner;
use qm_types::api::{AgentRef, CrewPhase, CrewRow, CrewTask, Waiting};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;

/// Said under the Quartermaster while there is no crew.
const NO_CREW: &str = "No crewmates yet.";

/// The most lines a task's name wraps to.
const NAME_LINES: usize = 2;

/// Draws the crew section into `area`, one line per task when `compact`.
pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    map: &mut FrameMap,
    compact: bool,
) {
    let heading = Span::styled("Crew", heading_style(app, Pane::Crew));
    let block = section_block(vec![heading], false);
    let built = build(app, block.inner(area).width, compact);
    map.crew_spots = built.spots;
    draw_list(
        frame,
        area,
        block,
        built.rows,
        app.view.crew_scroll,
        Pane::Crew,
        map,
    );
}

/// The list as drawn: its rows, and where each agent is in them, in the order the keys move.
pub(crate) struct Built {
    pub(crate) rows: Vec<ListRow>,
    /// Each agent's lines: the lines to keep in view when it is selected (a task's name and
    /// meta lines come with its first crewmate).
    pub(crate) spots: Vec<(Range<usize>, AgentRef)>,
}

/// The pane's row count at `inner_width`, which sizes the box.
pub(crate) fn height(app: &App, inner_width: u16, compact: bool) -> usize {
    build(app, inner_width, compact).rows.len()
}

/// One task as the list shows it: its crewmates' rows (under one supervisor), and who that is
/// when it isn't the Quartermaster.
struct Entry<'a> {
    task: Option<&'a CrewTask>,
    crew: Vec<&'a CrewRow>,
    via: Option<&'a str>,
}

impl Entry<'_> {
    /// The group it goes in: waiting on you counts as waiting; any crewmate still moving keeps it
    /// working; done only when every one is.
    fn phase(&self) -> CrewPhase {
        if self.crew.iter().any(|r| waiting(r).is_some()) {
            CrewPhase::Waiting
        } else if self.crew.iter().any(|r| r.phase == CrewPhase::Working) {
            CrewPhase::Working
        } else if self.crew.iter().any(|r| r.phase == CrewPhase::Waiting) {
            CrewPhase::Waiting
        } else {
            CrewPhase::Done
        }
    }

    /// The words it is listed under, never the legacy ID-bearing metadata label.
    fn name<'a>(&self, app: &'a App) -> &'a str {
        crewmate_name(app, self.crew[0].agent)
    }
}

/// A friendly crewmate/task name for app-owned chrome. Authored names and titles are kept as-is;
/// legacy `CrewRow::label` and `CrewTask::label` are metadata, not names.
pub(crate) fn crewmate_name(app: &App, agent: AgentRef) -> &str {
    app.snapshot
        .crew
        .iter()
        .find(|row| row.agent == agent)
        .and_then(|row| row.task.as_ref())
        .map(|task| task.name.as_str())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            app.snapshot
                .tasks
                .iter()
                .find(|detail| AgentRef::Crewmate { task: detail.task } == agent)
                .map(|detail| detail.title.as_str())
                .filter(|title| !title.is_empty())
        })
        .or_else(|| {
            app.snapshot
                .session
                .as_ref()
                .filter(|view| view.agent == agent)
                .map(|view| view.title.as_str())
                .filter(|title| !title.is_empty())
        })
        .unwrap_or("Crewmate")
}

/// The core's crewmate rows as entries, in its order: consecutive rows of one task under one
/// supervisor make one entry; a row without a task (an older server) is an entry of its own.
fn entries(crew: &[CrewRow]) -> Vec<Entry<'_>> {
    let mut out: Vec<Entry<'_>> = Vec::new();
    let mut via: Option<&str> = None;
    let mut last: Option<(u8, qm_types::ids::TaskLabelId)> = None;
    for row in crew {
        match row.agent {
            AgentRef::Quartermaster => continue,
            AgentRef::Secondmate { .. } => {
                via = Some(row.label.as_str());
                last = None;
                continue;
            }
            AgentRef::Crewmate { .. } => {}
        }
        let supervisor = if row.depth >= 2 { via } else { None };
        let key = row.task.as_ref().map(|t| (row.depth, t.id));
        match out.last_mut() {
            Some(entry) if key.is_some() && key == last => entry.crew.push(row),
            _ => out.push(Entry {
                task: row.task.as_ref(),
                crew: vec![row],
                via: supervisor,
            }),
        }
        last = key;
    }
    out
}

/// How task `label` reads in the crew list, for another list to show it the same way: its glyph,
/// its name, and its status (a team's crew count) in its colour. `None` when no crewmate of it is
/// listed.
pub(crate) fn task_mark(
    app: &App,
    label: qm_types::ids::TaskLabelId,
) -> Option<(Span<'static>, String, String, Style)> {
    let entries = entries(&app.snapshot.crew);
    let entry = entries
        .iter()
        .find(|e| e.task.is_some_and(|t| t.id == label))?;
    let first = entry.crew[0];
    let single = entry.crew.len() == 1;
    let glyph = if single {
        row_glyph(app, first)
    } else {
        phase_glyph(entry)
    };
    let status = if single {
        first.status.clone()
    } else {
        format!("{} crew", entry.crew.len())
    };
    Some((
        glyph,
        entry.name(app).to_owned(),
        status,
        status_style(first, single),
    ))
}

/// Whether `entry` shows only in Needs you: each of its crewmates waits on you for an item there.
fn in_needs_you(app: &App, entry: &Entry<'_>) -> bool {
    entry.crew.iter().all(|row| {
        let AgentRef::Crewmate { task } = row.agent else {
            return false;
        };
        waiting(row).is_some() && app.snapshot.actions.iter().any(|a| a.task == Some(task))
    })
}

/// The list's rows and spots at `inner_width`.
pub(crate) fn build(app: &App, inner_width: u16, compact: bool) -> Built {
    let inner = usize::from(inner_width);
    let mut out = Built {
        rows: Vec::new(),
        spots: Vec::new(),
    };
    let crew = &app.snapshot.crew;
    match crew.iter().find(|r| r.agent == AgentRef::Quartermaster) {
        Some(qm) => {
            let busy = qm
                .busy
                .then(|| spinner::glyph(app, spinner::Spinner::Lookout));
            let glyph = busy.unwrap_or_else(|| Span::styled("◆", theme::accent()));
            let status_style = if qm.status == "ready" {
                theme::ok()
            } else {
                theme::dim()
            };
            push_agent(
                app,
                &mut out,
                qm.agent,
                glyph,
                &qm.label,
                (&qm.status, status_style),
                inner,
                0,
                None,
            );
        }
        // The Quartermaster is always aboard, even before the core reports on it.
        None => out.rows.push(ListRow::plain(Line::from(vec![
            Span::styled("◆", theme::accent()),
            Span::raw(" Quartermaster"),
        ]))),
    }
    let shown: Vec<Entry<'_>> = entries(crew)
        .into_iter()
        .filter(|e| !in_needs_you(app, e))
        .collect();
    let secondmates: Vec<&CrewRow> = crew
        .iter()
        .filter(|r| matches!(r.agent, AgentRef::Secondmate { .. }))
        .collect();
    if shown.is_empty() && secondmates.is_empty() {
        let quiet = entries(crew).is_empty();
        if quiet {
            out.rows.extend(
                empty_lines(&[NO_CREW], inner_width)
                    .into_iter()
                    .map(ListRow::plain),
            );
        }
        return out;
    }
    for (phase, word, style) in [
        (CrewPhase::Working, "Working", theme::ok()),
        (CrewPhase::Waiting, "Waiting", theme::title()),
        (CrewPhase::Done, "Done", theme::title()),
    ] {
        let group: Vec<&Entry<'_>> = shown.iter().filter(|e| e.phase() == phase).collect();
        if group.is_empty() {
            continue;
        }
        out.rows.push(group_heading(word, style, group.len()));
        for entry in group {
            push_entry(app, &mut out, entry, inner, compact);
        }
    }
    if !secondmates.is_empty() {
        out.rows.push(group_heading(
            "Secondmates",
            theme::title(),
            secondmates.len(),
        ));
        for row in secondmates {
            let glyph = if row.busy {
                spinner::glyph(app, spinner::Spinner::Insignia)
            } else {
                Span::styled("○", theme::dim())
            };
            push_agent(
                app,
                &mut out,
                row.agent,
                glyph,
                &crate::presentation::Presenter(&app.snapshot).agent(row.agent),
                (&row.status, theme::dim()),
                inner,
                0,
                None,
            );
        }
    }
    out
}

/// A group's heading: `Working · 2`, the word in the group's colour and the count dim.
fn group_heading(word: &str, style: Style, count: usize) -> ListRow {
    ListRow::plain(Line::from(vec![
        Span::styled(word.to_owned(), style.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {count}"), theme::dim()),
    ]))
}

/// Whether `agent`'s rows are selected (`Some(true)` filled), and whether the pointer is over
/// them.
fn picked(app: &App, agent: AgentRef) -> (Option<bool>, bool) {
    let selected = (app.view.selected_crew == Some(agent)).then_some(!app.view.pointer_picked);
    let hovered = selected != Some(true) && app.term.hover == Some(Hit::Crew(agent));
    (selected, hovered)
}

fn label_style(hovered: bool) -> Style {
    if hovered {
        theme::text().add_modifier(Modifier::UNDERLINED)
    } else {
        theme::text()
    }
}

/// One agent's line, `indent` cells in: the glyph, its label and its status at the right edge;
/// the label is cut to leave the status room. Its spot runs from `from` (earlier lines that come
/// with it into view), else from its own line.
#[expect(
    clippy::too_many_arguments,
    reason = "one line's parts, each different for each kind of row"
)]
fn push_agent(
    app: &App,
    out: &mut Built,
    agent: AgentRef,
    glyph: Span<'static>,
    label: &str,
    (status, status_style): (&str, Style),
    inner: usize,
    indent: usize,
    from: Option<usize>,
) {
    let from = from.unwrap_or(out.rows.len());
    let (selected, hovered) = picked(app, agent);
    // Indent, glyph and space; then the label, at least one space, and the status.
    let lead = indent + 2;
    let room = inner.saturating_sub(lead + 1 + width(status)).max(1);
    let label = truncate(label, room);
    let gap = inner
        .saturating_sub(lead + width(&label) + width(status))
        .max(1);
    let line = Line::from(vec![
        Span::raw(" ".repeat(indent)),
        glyph,
        Span::raw(" "),
        Span::styled(label, label_style(hovered)),
        Span::raw(" ".repeat(gap)),
        Span::styled(status.to_owned(), status_style),
    ]);
    out.rows.push(ListRow {
        line,
        hit: Some(Hit::Crew(agent)),
        selected,
    });
    out.spots.push((from..out.rows.len(), agent));
}

/// One task's lines: its name, then its meta line, then its crewmates when it has more than one;
/// in `compact`, its name and status on one line.
fn push_entry(app: &App, out: &mut Built, entry: &Entry<'_>, inner: usize, compact: bool) {
    let first = entry.crew[0];
    let single = entry.crew.len() == 1;
    if compact {
        let status = if single {
            first.status.clone()
        } else {
            format!("{} crew", entry.crew.len())
        };
        let style = status_style(first, single);
        let glyph = if single {
            row_glyph(app, first)
        } else {
            phase_glyph(entry)
        };
        push_agent(
            app,
            out,
            first.agent,
            glyph,
            entry.name(app),
            (&status, style),
            inner,
            0,
            None,
        );
        return;
    }
    let from = out.rows.len();
    // A single crewmate's lines all select it; a team's name and meta lines are plain, and come
    // into view with its first crewmate.
    let (hit, selected, hovered) = if single {
        let (selected, hovered) = picked(app, first.agent);
        (Some(Hit::Crew(first.agent)), selected, hovered)
    } else {
        (None, None, false)
    };
    let glyph = if single {
        row_glyph(app, first)
    } else {
        phase_glyph(entry)
    };
    let mut name = wrap(entry.name(app), inner.saturating_sub(2).max(1));
    if name.len() > NAME_LINES {
        let rest = name[NAME_LINES - 1..].join(" ");
        name.truncate(NAME_LINES - 1);
        name.push(truncate(&rest, inner.saturating_sub(2).max(1)));
    }
    for (i, part) in name.into_iter().enumerate() {
        let lead = if i == 0 {
            vec![glyph.clone(), Span::raw(" ")]
        } else {
            vec![Span::raw("  ")]
        };
        let mut spans = lead;
        spans.push(Span::styled(part, label_style(hovered)));
        out.rows.push(ListRow {
            line: Line::from(spans),
            hit,
            selected,
        });
    }
    out.rows.push(ListRow {
        line: meta_line(entry, inner),
        hit,
        selected,
    });
    if single {
        out.spots.push((from..out.rows.len(), first.agent));
        return;
    }
    for (i, row) in entry.crew.iter().enumerate() {
        // Ordinals distinguish workers doing the same kind of work without exposing IDs.
        let kind = worker_kind(row);
        let count = entry.crew.iter().filter(|r| worker_kind(r) == kind).count();
        let label = if count > 1 {
            let number = entry.crew[..=i]
                .iter()
                .filter(|r| worker_kind(r) == kind)
                .count();
            format!("{kind} {number}")
        } else {
            kind.to_owned()
        };
        push_agent(
            app,
            out,
            row.agent,
            row_glyph(app, row),
            &label,
            (&row.status, status_style(row, true)),
            inner,
            2,
            (i == 0).then_some(from),
        );
    }
}

/// A worker's friendly role, including rows from older servers without a kind.
fn worker_kind(row: &CrewRow) -> &str {
    if row.kind.is_empty() {
        "Crewmate"
    } else {
        &row.kind
    }
}

/// The dim line under a task's name: `api-server · fix · via web-app`, with a single
/// crewmate's status at the right edge in its flag's colour. Parts that don't fit are left out
/// whole, never cut.
fn meta_line(entry: &Entry<'_>, inner: usize) -> Line<'static> {
    let first = entry.crew[0];
    let single = entry.crew.len() == 1;
    let room = inner.saturating_sub(2);
    let status = if single { first.status.as_str() } else { "" };
    // What fits beside the status: omit whole parts, the kind first, then who it came
    // through, then the project. The task's name is already above; no ID chip is needed.
    let via = entry.via.map(|s| format!("via {s}"));
    let mut parts: Vec<(u8, String)> = Vec::new();
    if let Some(task) = entry.task {
        parts.push((2, task.project.clone()));
    }
    if single {
        parts.push((0, first.kind.clone()));
    }
    if let Some(via) = via {
        parts.push((1, via));
    }
    parts.retain(|(_, p)| !p.is_empty());
    let sep = " · ";
    let joined = |parts: &[(u8, String)]| {
        parts
            .iter()
            .map(|(_, p)| p.as_str())
            .collect::<Vec<_>>()
            .join(sep)
    };
    let gap = usize::from(!status.is_empty());
    for drop in 0..3 {
        if width(&joined(&parts)) + gap + width(status) <= room {
            break;
        }
        parts.retain(|(rank, _)| *rank != drop);
    }
    let text = truncate(&joined(&parts), room.saturating_sub(gap + width(status)));
    let pad = if status.is_empty() {
        0
    } else {
        room.saturating_sub(width(&text) + width(status))
    };
    let status_style = match waiting(first) {
        Some(w) => waiting_style(w),
        None => theme::dim(),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(text, theme::dim()),
        Span::raw(" ".repeat(pad)),
        Span::styled(status.to_owned(), status_style),
    ])
}

/// A crewmate's glyph: its flag, else its busy indicator, else how its work stands.
fn row_glyph(app: &App, row: &CrewRow) -> Span<'static> {
    if let Some(w) = waiting(row) {
        return Span::styled("⚑", waiting_style(w));
    }
    if row.busy {
        return spinner::glyph(app, spinner::for_agent(&app.snapshot.crew, row.agent));
    }
    match row.phase {
        CrewPhase::Done => Span::styled("✓", theme::ok()),
        CrewPhase::Waiting => Span::styled("○", theme::dim()),
        _ if RESTING.contains(&row.status.as_str()) => Span::styled("○", theme::dim()),
        _ => Span::styled("●", theme::ok()),
    }
}

/// A team's glyph, from its group.
fn phase_glyph(entry: &Entry<'_>) -> Span<'static> {
    if let Some(w) = entry.crew.iter().find_map(|r| waiting(r)) {
        return Span::styled("⚑", waiting_style(w));
    }
    match entry.phase() {
        CrewPhase::Done => Span::styled("✓", theme::ok()),
        CrewPhase::Waiting => Span::styled("○", theme::dim()),
        _ => Span::styled("●", theme::ok()),
    }
}

/// A status's colour: its flag's while it waits on you, else dim.
fn status_style(row: &CrewRow, single: bool) -> Style {
    match waiting(row) {
        Some(w) if single => waiting_style(w),
        _ => theme::dim(),
    }
}

/// What a row is waiting on you for, if anything (`needs_you` alone reads as an approval).
fn waiting(row: &CrewRow) -> Option<Waiting> {
    row.waiting
        .or_else(|| row.needs_you.then_some(Waiting::Approval))
}

/// Statuses of crewmates doing nothing right now, from a server that doesn't say their phase:
/// drawn with a dim `○`.
const RESTING: &[&str] = &["idle", "queued", "waits", "reported", "landed"];

fn waiting_style(waiting: Waiting) -> Style {
    match waiting {
        Waiting::Decision => theme::warn(),
        _ => theme::accent(),
    }
}
