//! The context block at the head of every Quartermaster turn.
//!
//! Tagged lines, rebuilt in full every turn, compact and capped. Rendering is pure: the core
//! gathers [`ContextFacts`] from the store and [`render`] returns the text, in a fixed format: the
//! root tag with `turn`, `at` and `tz`; then `<projects>`, `<workers>`, `<tasks>`, the three "since
//! your last turn" sections (`<you>`, `<activity>`, `<model_changes>`), `<recent_outcomes>`,
//! `<standing_instructions>` and `<models>`. Columns are padded to the widest value in their
//! section, two spaces apart.
//!
//! - **Caps.** Open and active tasks only, at most [`TASKS_PER_PROJECT`] per project and
//!   [`TASKS_TOTAL`] in all; workers the same. A cut list ends with one overflow line per project
//!   cut (`…and 9 more in web-app; use find_tasks for the rest`) and one for the total cap
//!   (`…and 4 more; use find_tasks for the rest`). Each since-section shows at most
//!   [`SINCE_LINES`] lines, newest first, then `…and N more`. Recent outcomes: the
//!   [`RECENT_OUTCOMES`] newest.
//! - **Long text is cut** with `…`: titles at [`TITLE_CHARS`], event lines and other free text at
//!   [`LINE_CHARS`], standing instructions at [`INSTRUCTION_CHARS`]. Line breaks become spaces, so
//!   every entry stays one line.
//! - **Empty sections.** `<projects>`, `<workers>`, `<tasks>`, `<standing_instructions>` and
//!   `<models>` always appear, self-closing when empty; the since-sections and
//!   `<recent_outcomes>` are left out when empty.
//!
//! Text from outside the app (titles, scopes, your instructions) reaches this module already
//! neutralised by the core, so nothing here can open or close a marker.

use jiff::Zoned;
use qm_types::domain::{
    Autonomy, DeliveryMode, Effort, TaskKind, TaskStatus, Tier, VerificationLevel,
};
use qm_types::ids::{SecondmateId, StandingInstructionId, TaskId, TurnId};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Open and active tasks shown per project.
pub const TASKS_PER_PROJECT: usize = 12;
/// Open and active tasks shown in all.
pub const TASKS_TOTAL: usize = 36;
/// Workers shown per project.
pub const WORKERS_PER_PROJECT: usize = 12;
/// Workers shown in all.
pub const WORKERS_TOTAL: usize = 36;
/// Lines per "since your last turn" section.
pub const SINCE_LINES: usize = 20;
/// Recent outcomes shown.
pub const RECENT_OUTCOMES: usize = 5;
/// Standing instructions shown.
pub const STANDING_MAX: usize = 30;
/// A task title's length before it is cut.
pub const TITLE_CHARS: usize = 60;
/// An event line's (and any other free text's) length before it is cut.
pub const LINE_CHARS: usize = 120;
/// A standing instruction's length before it is cut.
pub const INSTRUCTION_CHARS: usize = 200;

/// Recent outcomes per row, and the gap between them.
const OUTCOMES_PER_ROW: usize = 3;
/// Tiers per row in `<models>`.
const TIERS_PER_ROW: usize = 2;
/// The gap between the cells of a multi-column row (`<recent_outcomes>`, `<models>`).
const GUTTER: &str = "     ";
/// The gap before a worker's note ("reviews T-0040").
const NOTE_GAP: &str = "   ";
/// The gap between columns.
const GAP: &str = "  ";

/// Everything the block shows, gathered by the core for one turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFacts {
    pub turn: TurnId,
    /// Now, in your time zone: `at="2026-10-18T14:02-04:00" tz="America/Toronto"`.
    pub at: Zoned,
    /// Registered projects, then the built-in workspace.
    pub projects: Vec<ProjectFact>,
    /// In tree order: the Quartermaster's crewmates, then each secondmate followed by its crew.
    pub workers: Vec<WorkerFact>,
    /// Every working agent, before the caps (`of="…"`).
    pub workers_total: usize,
    /// Open and active tasks only, newest first (the caps keep the newest).
    pub tasks: Vec<TaskFact>,
    /// Every open and active task, before the caps (`open="…"`).
    pub tasks_open: usize,
    /// The previous turn, which the since-sections count from; `None` on the first turn.
    pub since_turn: Option<TurnId>,
    /// Your recent actions and choices (`you.*` events).
    pub you: Since,
    /// New replies and activity on tasks (`task.*`, `pr.*`, `checks.*`, `secondmate.*`).
    pub activity: Since,
    /// Changes to the models available (`model.*`).
    pub model_changes: Since,
    /// The newest outcomes, newest first.
    pub recent_outcomes: Vec<EventLine>,
    /// Your standing instructions, oldest first.
    pub standing: Vec<(StandingInstructionId, String)>,
    /// The models in each tier, strongest first.
    pub models: Vec<TierModels>,
}

/// A registered project, or the built-in workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFact {
    pub name: String,
    /// With `~` for your home folder; `(built-in workspace)` for the workspace.
    pub path: String,
    pub delivery: DeliveryMode,
    pub autonomy: Autonomy,
    pub verify: VerificationLevel,
}

/// A row of `<workers>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerFact {
    Crewmate(CrewmateFact),
    Secondmate(SecondmateFact),
}

/// A crewmate at work: `T-0042  crewmate  api-server  fix  running  12m  medium · sonnet-5-5`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrewmateFact {
    pub task: TaskId,
    /// The secondmate it works under: shown indented, as "crew of SM-02".
    pub under: Option<SecondmateId>,
    pub project: String,
    pub kind: TaskKind,
    /// One or two words: "running", "waiting".
    pub status: String,
    /// How long it has been at it, in whole minutes.
    pub minutes: u64,
    pub effort: Option<Effort>,
    pub model: Option<String>,
    /// A few words after the model: "reviews T-0040".
    pub note: Option<String>,
}

/// A secondmate: `SM-02  secondmate  scope="web-app frontend"  idle  base=release/2.4`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondmateFact {
    pub id: SecondmateId,
    pub scope: String,
    pub status: String,
    pub base_branch: Option<String>,
}

/// An open or active task: `T-0042  api-server  fix(auth): token refresh race  running`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFact {
    pub task: TaskId,
    pub project: String,
    pub title: String,
    pub status: TaskStatus,
    /// The one-line state when the app knows more than the status ("PR #212 ready · 14/14 · held
    /// for your approval @a1b2c3d"). See [`task_line`].
    pub detail: Option<String>,
}

/// One line of a since-section or of the recent outcomes: `14:01  T-0043 reported (plan)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventLine {
    pub at: Zoned,
    pub text: String,
}

/// A "since your last turn" section's lines, newest first, and how many more there were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Since {
    pub lines: Vec<EventLine>,
    /// Lines left out before they reached the renderer.
    pub more: usize,
}

/// The models in one tier: `high  opus-5-5 · gpt-6-sol`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierModels {
    pub tier: Tier,
    pub models: Vec<String>,
    /// In parentheses after the models: "only when you ask" for premium.
    pub note: Option<String>,
}

/// The note on the premium tier: it is never picked automatically.
pub const PREMIUM_NOTE: &str = "only when you ask";

/// A task's one-line state: the detail when the app has it, else its status in words.
pub fn task_line(task: &TaskFact) -> String {
    match &task.detail {
        Some(detail) => cut(detail, LINE_CHARS),
        None => status_words(task.status).to_owned(),
    }
}

/// A task status as the block words it.
pub const fn status_words(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Preparing => "preparing",
        TaskStatus::Running => "running",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Held => "held",
        TaskStatus::Stuck => "stuck",
        TaskStatus::Reported => "reported",
        TaskStatus::InReview => "in review",
        TaskStatus::Settling => "settling",
        TaskStatus::Landed => "landed",
        TaskStatus::Closed => "closed",
        TaskStatus::Failed => "failed",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Discarded => "discarded",
    }
}

/// The block, ending in a newline.
pub fn render(facts: &ContextFacts) -> String {
    let mut out = String::new();
    let tz = facts
        .at
        .time_zone()
        .iana_name()
        .map_or_else(|| facts.at.strftime("%:z").to_string(), str::to_owned);
    let _ = writeln!(
        out,
        "<qm:context turn=\"{}\" at=\"{}\" tz=\"{}\">",
        facts.turn,
        facts.at.strftime("%Y-%m-%dT%H:%M%:z"),
        attr(&tz)
    );
    projects(&mut out, &facts.projects);
    workers(&mut out, facts);
    tasks(&mut out, facts);
    let since_attr = facts
        .since_turn
        .map(|t| format!(" since_turn=\"{t}\""))
        .unwrap_or_default();
    since(&mut out, "you", &since_attr, &facts.you);
    since(&mut out, "activity", &since_attr, &facts.activity);
    since(&mut out, "model_changes", &since_attr, &facts.model_changes);
    recent_outcomes(&mut out, &facts.recent_outcomes);
    standing(&mut out, &facts.standing);
    models(&mut out, &facts.models);
    out.push_str("</qm:context>\n");
    out
}

// ---- sections ----

fn projects(out: &mut String, projects: &[ProjectFact]) {
    if projects.is_empty() {
        out.push_str("<projects/>\n");
        return;
    }
    // The delivery column is as wide as its widest possible value, so a project added later
    // doesn't shift the columns after it.
    let delivery_min = [
        DeliveryMode::DirectPr,
        DeliveryMode::BranchOnly,
        DeliveryMode::BuildInPlace,
        DeliveryMode::Folder,
    ]
    .iter()
    .map(|d| format!("delivery={d}").chars().count())
    .max()
    .unwrap_or(0);
    let rows: Vec<Vec<Cell>> = projects
        .iter()
        .map(|p| {
            vec![
                Cell::left(cut(&p.name, TITLE_CHARS)),
                Cell::left(cut(&p.path, LINE_CHARS)),
                Cell::left(format!("delivery={}", p.delivery)).at_least(delivery_min),
                Cell::left(format!("autonomy={}", p.autonomy)),
                Cell::left(format!("verify={}", p.verify)),
            ]
        })
        .collect();
    out.push_str("<projects>\n");
    for line in table(&rows) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("</projects>\n");
}

/// Applies a per-project and a total cap to items in order: which to show, and the overflow
/// lines.
fn capped<T>(
    items: &[T],
    project: fn(&T) -> Option<&str>,
    per_project: usize,
    total: usize,
    total_before_caps: usize,
) -> (Vec<&T>, Vec<String>) {
    let mut shown = Vec::new();
    let mut per: BTreeMap<&str, usize> = BTreeMap::new();
    // Projects in the order their first cut happened, with how many were cut.
    let mut cut_in: Vec<(&str, usize)> = Vec::new();
    for item in items {
        let p = project(item);
        if let Some(p) = p {
            let n = per.entry(p).or_default();
            if *n >= per_project {
                match cut_in.iter_mut().find(|(name, _)| *name == p) {
                    Some((_, k)) => *k += 1,
                    None => cut_in.push((p, 1)),
                }
                continue;
            }
            if shown.len() >= total {
                continue;
            }
            *n += 1;
        } else if shown.len() >= total {
            continue;
        }
        shown.push(item);
    }
    let cut_per_project: usize = cut_in.iter().map(|(_, k)| k).sum();
    let mut lines: Vec<String> = cut_in
        .iter()
        .map(|(p, k)| format!("…and {k} more in {p}; use find_tasks for the rest"))
        .collect();
    let rest = total_before_caps
        .max(items.len())
        .saturating_sub(shown.len() + cut_per_project);
    if rest > 0 {
        lines.push(format!("…and {rest} more; use find_tasks for the rest"));
    }
    (shown, lines)
}

fn worker_project(w: &WorkerFact) -> Option<&str> {
    match w {
        WorkerFact::Crewmate(c) => Some(c.project.as_str()),
        WorkerFact::Secondmate(_) => None,
    }
}

fn workers(out: &mut String, facts: &ContextFacts) {
    let (shown, overflow) = capped(
        &facts.workers,
        worker_project,
        WORKERS_PER_PROJECT,
        WORKERS_TOTAL,
        facts.workers_total,
    );
    let total = facts.workers_total.max(facts.workers.len());
    if shown.is_empty() && overflow.is_empty() {
        let _ = writeln!(out, "<workers shown=\"0\" of=\"{total}\"/>");
        return;
    }
    let _ = writeln!(out, "<workers shown=\"{}\" of=\"{total}\">", shown.len());

    // The first two columns (ID and role) line up across every top-level row; the rest of a
    // crewmate's columns line up across the crewmates of its group (the Quartermaster's own, or
    // one secondmate's crew).
    let top: Vec<&WorkerFact> = shown
        .iter()
        .copied()
        .filter(|w| !matches!(w, WorkerFact::Crewmate(c) if c.under.is_some()))
        .collect();
    let id_width = top.iter().map(|w| width(&worker_id(w))).max().unwrap_or(0);
    let role_width = top.iter().map(|w| width(worker_role(w))).max().unwrap_or(0);

    let mut groups: BTreeMap<Option<SecondmateId>, Vec<&CrewmateFact>> = BTreeMap::new();
    for w in &shown {
        if let WorkerFact::Crewmate(c) = w {
            groups.entry(c.under).or_default().push(c);
        }
    }
    let group_widths: BTreeMap<Option<SecondmateId>, Vec<usize>> = groups
        .iter()
        .map(|(under, crew)| {
            let rows: Vec<Vec<Cell>> = crew.iter().map(|c| crew_cells(c)).collect();
            (*under, widths(&rows))
        })
        .collect();

    for w in &shown {
        let line = match w {
            WorkerFact::Crewmate(c) => {
                let widths = &group_widths[&c.under];
                let mut line = String::new();
                if c.under.is_some() {
                    line.push_str(GAP);
                } else {
                    line.push_str(&pad(&c.task.to_string(), id_width, Align::Left));
                    line.push_str(GAP);
                    line.push_str(&pad("crewmate", role_width, Align::Left));
                    line.push_str(GAP);
                }
                let cells = crew_cells(c);
                let (head, rest) = if c.under.is_some() {
                    (&cells[..], &widths[..])
                } else {
                    (&cells[1..], &widths[1..])
                };
                line.push_str(&join(head, rest));
                let model = model_text(c);
                if let Some(model) = &model {
                    line.push_str(GAP);
                    line.push_str(model);
                }
                if let Some(note) = &c.note {
                    line.push_str(if model.is_some() { NOTE_GAP } else { GAP });
                    line.push_str(&cut(note, LINE_CHARS));
                }
                line
            }
            WorkerFact::Secondmate(s) => {
                let mut line = format!(
                    "{}{GAP}{}{GAP}scope=\"{}\"{GAP}{}",
                    pad(&s.id.to_string(), id_width, Align::Left),
                    pad("secondmate", role_width, Align::Left),
                    attr(&cut(&s.scope, TITLE_CHARS)),
                    s.status
                );
                if let Some(base) = &s.base_branch {
                    let _ = write!(line, "{GAP}base={}", cut(base, TITLE_CHARS));
                }
                line
            }
        };
        out.push_str(line.trim_end());
        out.push('\n');
    }
    for line in overflow {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("</workers>\n");
}

fn worker_id(w: &WorkerFact) -> String {
    match w {
        WorkerFact::Crewmate(c) => c.task.to_string(),
        WorkerFact::Secondmate(s) => s.id.to_string(),
    }
}

fn worker_role(w: &WorkerFact) -> &'static str {
    match w {
        WorkerFact::Crewmate(_) => "crewmate",
        WorkerFact::Secondmate(_) => "secondmate",
    }
}

/// A crewmate's aligned columns: for a secondmate's crew, ID, "crew of SM-02", project, kind,
/// status, time; for the Quartermaster's own, the same with the role in place of "crew of …"
/// (the ID and role columns are then aligned with the other top-level rows instead).
fn crew_cells(c: &CrewmateFact) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(6);
    match c.under {
        Some(sm) => {
            cells.push(Cell::left(c.task.to_string()));
            cells.push(Cell::left(format!("crew of {sm}")));
        }
        None => cells.push(Cell::left(c.task.to_string())),
    }
    cells.push(Cell::left(cut(&c.project, TITLE_CHARS)));
    cells.push(Cell::left(c.kind.to_string()));
    cells.push(Cell::left(c.status.clone()));
    cells.push(Cell::right(elapsed(c.minutes)));
    cells
}

fn model_text(c: &CrewmateFact) -> Option<String> {
    match (&c.effort, &c.model) {
        (Some(effort), Some(model)) => Some(format!("{effort} · {model}")),
        (None, Some(model)) => Some(model.clone()),
        _ => None,
    }
}

/// "12m", "3h", "2d".
fn elapsed(minutes: u64) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (24 * 60))
    }
}

fn tasks(out: &mut String, facts: &ContextFacts) {
    let (shown, overflow) = capped(
        &facts.tasks,
        |t| Some(t.project.as_str()),
        TASKS_PER_PROJECT,
        TASKS_TOTAL,
        facts.tasks_open,
    );
    let open = facts.tasks_open.max(facts.tasks.len());
    if shown.is_empty() && overflow.is_empty() {
        let _ = writeln!(out, "<tasks open=\"{open}\" shown=\"0\"/>");
        return;
    }
    let _ = writeln!(out, "<tasks open=\"{open}\" shown=\"{}\">", shown.len());
    // The facts are newest first, so the caps keep the newest; the rows then read in creation
    // order, oldest first, like the workers and the projects.
    let rows: Vec<Vec<Cell>> = shown
        .iter()
        .rev()
        .map(|t| {
            vec![
                Cell::left(t.task.to_string()),
                Cell::left(cut(&t.project, TITLE_CHARS)),
                Cell::left(cut(&t.title, TITLE_CHARS)),
                Cell::left(task_line(t)),
            ]
        })
        .collect();
    for line in table(&rows) {
        out.push_str(&line);
        out.push('\n');
    }
    for line in overflow {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("</tasks>\n");
}

fn since(out: &mut String, tag: &str, since_attr: &str, section: &Since) {
    if section.lines.is_empty() && section.more == 0 {
        return;
    }
    let _ = writeln!(out, "<{tag}{since_attr}>");
    for line in section.lines.iter().take(SINCE_LINES) {
        let _ = writeln!(out, "{}", event_line(line));
    }
    let more = section.more + section.lines.len().saturating_sub(SINCE_LINES);
    if more > 0 {
        let _ = writeln!(out, "…and {more} more");
    }
    let _ = writeln!(out, "</{tag}>");
}

fn event_line(line: &EventLine) -> String {
    format!(
        "{}{GAP}{}",
        line.at.strftime("%H:%M"),
        cut(&line.text, LINE_CHARS)
    )
}

fn recent_outcomes(out: &mut String, outcomes: &[EventLine]) {
    let shown: Vec<String> = outcomes
        .iter()
        .take(RECENT_OUTCOMES)
        .map(event_line)
        .collect();
    if shown.is_empty() {
        return;
    }
    let _ = writeln!(out, "<recent_outcomes shown=\"{}\">", shown.len());
    for line in grid(&shown, OUTCOMES_PER_ROW) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("</recent_outcomes>\n");
}

fn standing(out: &mut String, standing: &[(StandingInstructionId, String)]) {
    if standing.is_empty() {
        out.push_str("<standing_instructions/>\n");
        return;
    }
    out.push_str("<standing_instructions>\n");
    let rows: Vec<Vec<Cell>> = standing
        .iter()
        .take(STANDING_MAX)
        .map(|(id, text)| {
            vec![
                Cell::left(id.to_string()),
                Cell::left(cut(text, INSTRUCTION_CHARS)),
            ]
        })
        .collect();
    for line in table(&rows) {
        out.push_str(&line);
        out.push('\n');
    }
    let more = standing.len().saturating_sub(STANDING_MAX);
    if more > 0 {
        let _ = writeln!(out, "…and {more} more");
    }
    out.push_str("</standing_instructions>\n");
}

fn models(out: &mut String, tiers: &[TierModels]) {
    let tiers: Vec<&TierModels> = tiers.iter().filter(|t| !t.models.is_empty()).collect();
    if tiers.is_empty() {
        out.push_str("<models/>\n");
        return;
    }
    // Each grid column pads its own tier names: "premium" and "medium" in the first, "high" and
    // "low" in the second.
    let mut tier_width = [0; TIERS_PER_ROW];
    for (i, t) in tiers.iter().enumerate() {
        let col = i % TIERS_PER_ROW;
        tier_width[col] = tier_width[col].max(width(t.tier.as_str()));
    }
    let cells: Vec<String> = tiers
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut cell = format!(
                "{}{GAP}{}",
                pad(t.tier.as_str(), tier_width[i % TIERS_PER_ROW], Align::Left),
                t.models
                    .iter()
                    .map(|m| cut(m, TITLE_CHARS))
                    .collect::<Vec<_>>()
                    .join(" · ")
            );
            if let Some(note) = &t.note {
                let _ = write!(cell, " ({note})");
            }
            cell
        })
        .collect();
    out.push_str("<models>\n");
    for line in grid(&cells, TIERS_PER_ROW) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("</models>\n");
}

// ---- layout ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

#[derive(Clone, Debug)]
struct Cell {
    text: String,
    align: Align,
    min: usize,
}

impl Cell {
    fn left(text: String) -> Self {
        Self {
            text,
            align: Align::Left,
            min: 0,
        }
    }

    fn right(text: String) -> Self {
        Self {
            text,
            align: Align::Right,
            min: 0,
        }
    }

    fn at_least(mut self, min: usize) -> Self {
        self.min = min;
        self
    }
}

/// Characters, which is what the block's alignment counts (everything in it is one cell wide).
fn width(s: &str) -> usize {
    s.chars().count()
}

fn pad(s: &str, to: usize, align: Align) -> String {
    let fill = " ".repeat(to.saturating_sub(width(s)));
    match align {
        Align::Left => format!("{s}{fill}"),
        Align::Right => format!("{fill}{s}"),
    }
}

/// Each column's width: its widest cell (or its minimum).
fn widths(rows: &[Vec<Cell>]) -> Vec<usize> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|cell| width(&cell.text).max(cell.min))
                .max()
                .unwrap_or(0)
        })
        .collect()
}

/// One row's cells padded to `widths`, two spaces apart; the last cell isn't padded on the right.
fn join(cells: &[Cell], widths: &[usize]) -> String {
    let last = cells.len().saturating_sub(1);
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str(GAP);
        }
        if i == last && cell.align == Align::Left {
            line.push_str(&cell.text);
        } else {
            line.push_str(&pad(&cell.text, widths[i], cell.align));
        }
    }
    line
}

/// Rows with their columns aligned.
fn table(rows: &[Vec<Cell>]) -> Vec<String> {
    let widths = widths(rows);
    rows.iter()
        .map(|r| join(r, &widths).trim_end().to_owned())
        .collect()
}

/// Cells laid out `per_row` to a row, each grid column padded to its widest cell, a wide gap
/// between them.
fn grid(cells: &[String], per_row: usize) -> Vec<String> {
    let mut col_width = vec![0; per_row];
    for (i, cell) in cells.iter().enumerate() {
        let col = i % per_row;
        col_width[col] = col_width[col].max(width(cell));
    }
    cells
        .chunks(per_row)
        .map(|row| {
            let last = row.len() - 1;
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    line.push_str(GUTTER);
                }
                if i == last {
                    line.push_str(cell);
                } else {
                    line.push_str(&pad(cell, col_width[i], Align::Left));
                }
            }
            line
        })
        .collect()
}

/// One line, at most `max` characters: line breaks become spaces, and a longer text is cut with
/// `…`.
pub fn cut(text: &str, max: usize) -> String {
    let flat: String = text
        .split(['\n', '\r', '\t'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let flat = flat.trim();
    if width(flat) <= max {
        return flat.to_owned();
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// A value inside a double-quoted attribute.
fn attr(text: &str) -> String {
    text.replace('"', "'")
}
