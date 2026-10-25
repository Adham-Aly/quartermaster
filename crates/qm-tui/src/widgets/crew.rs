//! The crew tree: the Quartermaster at the root; below it its tasks (`T-`), each with its
//! crewmates, then its secondmates, each with its own tasks and their crewmates. Agents carry a
//! status glyph (the busy indicator while the agent is in a turn) and an aligned status column;
//! a task's heading carries neither, only the guide lines that join it to its crewmates.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, heading_style, section_block};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width};
use crate::theme;
use crate::widgets::spinner;
use qm_types::api::{AgentRef, CrewRow, Waiting};
use qm_types::ids::TaskLabelId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Draws the crew section into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let heading = Span::styled("Crew", heading_style(app, Pane::Crew));
    let block = section_block(vec![heading], true);
    let rows = rows(app, block.inner(area).width);
    draw_list(
        frame,
        area,
        block,
        rows,
        app.view.crew_scroll,
        Pane::Crew,
        map,
    );
}

/// One line of the tree as drawn: an agent's row, or the heading of a task over its crewmates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Node<'a> {
    pub(crate) depth: u8,
    pub(crate) label: &'a str,
    /// The agent's row; `None` on a task's heading.
    pub(crate) row: Option<&'a CrewRow>,
}

impl Node<'_> {
    fn status(&self) -> &str {
        self.row.map_or("", |r| r.status.as_str())
    }
}

/// The tree as drawn from the core's rows: a crewmate with a task one level deeper than its
/// row says, under a heading drawn above the first of its task's crewmates that come together.
pub(crate) fn nodes(crew: &[CrewRow]) -> Vec<Node<'_>> {
    let mut out = Vec::with_capacity(crew.len());
    // The heading the rows now go under: its depth and task.
    let mut heading: Option<(u8, TaskLabelId)> = None;
    for row in crew {
        let Some(task) = &row.task else {
            heading = None;
            out.push(Node {
                depth: row.depth,
                label: &row.label,
                row: Some(row),
            });
            continue;
        };
        if heading != Some((row.depth, task.id)) {
            heading = Some((row.depth, task.id));
            out.push(Node {
                depth: row.depth,
                label: &task.label,
                row: None,
            });
        }
        out.push(Node {
            depth: row.depth.saturating_add(1),
            label: &row.label,
            row: Some(row),
        });
    }
    out
}

/// Where each agent is drawn: its line in [`nodes`], in the core's order.
pub(crate) fn agent_lines(crew: &[CrewRow]) -> Vec<(usize, AgentRef)> {
    nodes(crew)
        .iter()
        .enumerate()
        .filter_map(|(i, node)| node.row.map(|row| (i, row.agent)))
        .collect()
}

/// The pane's rows at `inner_width`; their count sizes the box.
pub(crate) fn rows(app: &App, inner_width: u16) -> Vec<ListRow> {
    let crew = &app.snapshot.crew;
    if crew.is_empty() {
        // The Quartermaster is always aboard, even before the core reports on it.
        let mut out = vec![ListRow::plain(Line::from(vec![
            Span::styled("◆", theme::accent()),
            Span::raw(" Quartermaster"),
        ]))];
        out.extend(
            empty_lines(&["No crewmates yet."], inner_width)
                .into_iter()
                .map(ListRow::plain),
        );
        return out;
    }
    let tree = nodes(crew);
    let width_in = usize::from(inner_width);
    let columns = fit_columns(&tree, status_columns(&tree), width_in);
    let mut out: Vec<ListRow> = tree
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let Some(row) = node.row else {
                return ListRow::plain(heading_line(&tree, i, width_in));
            };
            let hit = Hit::Crew(row.agent);
            let selected =
                (app.view.selected_crew == Some(row.agent)).then_some(!app.view.pointer_picked);
            let hovered = selected != Some(true) && app.term.hover == Some(hit);
            // A flag wins over the busy indicator, which then isn't drawn at all.
            let busy = (row.busy && waiting(row).is_none())
                .then(|| spinner::glyph(app, spinner::Spinner::of(row.agent)));
            ListRow {
                line: line(&tree, i, columns[i], hovered, busy),
                hit: Some(hit),
                selected,
            }
        })
        .collect();
    // The Quartermaster alone: still no crewmates, whatever its status says.
    if crew.iter().all(|r| r.agent == AgentRef::Quartermaster) {
        out.extend(
            empty_lines(&["No crewmates yet."], inner_width)
                .into_iter()
                .map(ListRow::plain),
        );
    }
    out
}

/// Caps the status columns so every status fits inside `inner_width`. Lines that align together
/// move left together, as far as their widest status needs; a label that would then be cut keeps
/// its line's column where its own status allows, so no label is cut further than its own status
/// asks.
fn fit_columns(nodes: &[Node<'_>], columns: Vec<usize>, inner_width: usize) -> Vec<usize> {
    let groups: Vec<Option<(u8, Option<usize>)>> =
        (0..nodes.len()).map(|i| align_group(nodes, i)).collect();
    columns
        .into_iter()
        .enumerate()
        .map(|(i, column)| {
            let node = &nodes[i];
            let widest = (0..nodes.len())
                .filter(|&j| groups[j].is_some() && groups[j] == groups[i])
                .map(|j| width(nodes[j].status()))
                .max()
                .unwrap_or(0);
            let aligned = column.min(inner_width.saturating_sub(widest));
            let end = prefix_width(node) + width(node.label);
            let at = if aligned > end {
                aligned
            } else {
                column.min(inner_width.saturating_sub(width(node.status())))
            };
            at.max(prefix_width(node) + 2)
        })
        .collect()
}

/// The cells before a line's label: `◆ ` at the root, `├ ● ` at depth 1, two more per level.
fn prefix_width(node: &Node<'_>) -> usize {
    match node.depth {
        0 => 2,
        d => 2 * usize::from(d) + 2,
    }
}

/// The index of each line's parent: the nearest earlier line one level up.
fn parent_of(nodes: &[Node<'_>], i: usize) -> Option<usize> {
    let depth = nodes[i].depth;
    if depth == 0 {
        return None;
    }
    nodes[..i].iter().rposition(|n| n.depth == depth - 1)
}

/// The agent line `i` reports to: its parent, or past a task's heading, the heading's parent.
fn owner_of(nodes: &[Node<'_>], i: usize) -> Option<usize> {
    let mut at = parent_of(nodes, i);
    while let Some(j) = at {
        if nodes[j].row.is_some() {
            return Some(j);
        }
        at = parent_of(nodes, j);
    }
    None
}

/// The lines whose statuses share a column: agents at one depth reporting to one agent, so a
/// supervisor's crewmates line up across its tasks. `None` for a task's heading, which has none.
fn align_group(nodes: &[Node<'_>], i: usize) -> Option<(u8, Option<usize>)> {
    nodes[i].row.map(|_| (nodes[i].depth, owner_of(nodes, i)))
}

/// The cell each line's status starts at: 2 cells after the longest prefix-plus-label among the
/// lines it aligns with ([`align_group`]); a root's status one cell left of the column of the
/// first agent reporting to it, or 2 after its label when it has none. A task's heading has no
/// status, and its label moves no column.
fn status_columns(nodes: &[Node<'_>]) -> Vec<usize> {
    let groups: Vec<Option<(u8, Option<usize>)>> =
        (0..nodes.len()).map(|i| align_group(nodes, i)).collect();
    let group_column = |i: usize| {
        (0..nodes.len())
            .filter(|&j| groups[j].is_some() && groups[j] == groups[i])
            .map(|j| prefix_width(&nodes[j]) + width(nodes[j].label))
            .max()
            .unwrap_or(0)
            + 2
    };
    (0..nodes.len())
        .map(|i| {
            let node = &nodes[i];
            let end = prefix_width(node) + width(node.label);
            if node.depth > 0 {
                return group_column(i);
            }
            let first_report =
                (0..nodes.len()).find(|&j| nodes[j].row.is_some() && owner_of(nodes, j) == Some(i));
            match first_report {
                Some(report) => (group_column(report) - 1).max(end + 1),
                None => end + 2,
            }
        })
        .collect()
}

/// Whether no later sibling follows line `i` (it draws `└`, not `├`).
fn is_last(nodes: &[Node<'_>], i: usize) -> bool {
    let depth = nodes[i].depth;
    nodes[i + 1..]
        .iter()
        .take_while(|n| n.depth >= depth)
        .all(|n| n.depth > depth)
}

/// The guides before line `i`'s branch: for each level above it, `│ ` where that ancestor has a
/// later sibling (its line runs on past this one), else blank.
fn guides(nodes: &[Node<'_>], i: usize) -> String {
    let mut ancestors = Vec::new();
    let mut at = parent_of(nodes, i);
    while let Some(j) = at {
        if nodes[j].depth == 0 {
            break;
        }
        ancestors.push(j);
        at = parent_of(nodes, j);
    }
    ancestors
        .iter()
        .rev()
        .map(|&j| if is_last(nodes, j) { "  " } else { "│ " })
        .collect()
}

/// The guides and branch before a line's glyph: `│ ├ `.
fn branch_spans(nodes: &[Node<'_>], i: usize) -> Vec<Span<'static>> {
    let branch = if is_last(nodes, i) { "└" } else { "├" };
    vec![
        Span::styled(guides(nodes, i), theme::dim()),
        Span::styled(branch, theme::dim()),
        Span::raw(" "),
    ]
}

/// What a row is waiting on you for, if anything (`needs_you` alone reads as an approval).
fn waiting(row: &CrewRow) -> Option<Waiting> {
    row.waiting
        .or_else(|| row.needs_you.then_some(Waiting::Approval))
}

/// Statuses of agents doing nothing right now, drawn with a dim `○`: an idle secondmate, and a
/// task that is queued, waits, has reported or has landed. A flagged row
/// keeps its `⚑`; a busy row (in a turn) spins; every other row is working and gets the green
/// `●`.
const RESTING: &[&str] = &["idle", "queued", "waits", "reported", "landed"];

fn waiting_style(waiting: Waiting) -> Style {
    match waiting {
        Waiting::Decision => theme::warn(),
        _ => theme::accent(),
    }
}

/// A task's heading, `│ ├ ◇ T-0042 api-server`: a dim glyph and the task's words, cut to
/// `inner_width`; it has no status.
fn heading_line(nodes: &[Node<'_>], i: usize, inner_width: usize) -> Line<'static> {
    let node = &nodes[i];
    let mut spans = branch_spans(nodes, i);
    spans.push(Span::styled("◇", theme::dim()));
    spans.push(Span::raw(" "));
    let room = inner_width.saturating_sub(prefix_width(node)).max(1);
    spans.push(Span::styled(truncate(node.label, room), theme::text()));
    Line::from(spans)
}

/// An agent's line of the tree, its status starting at cell `column`; `busy` is the busy
/// indicator's glyph when the agent is in a turn, drawn in place of the row's own (a flag still
/// wins).
fn line(
    nodes: &[Node<'_>],
    i: usize,
    column: usize,
    hovered: bool,
    busy: Option<Span<'static>>,
) -> Line<'static> {
    let node = &nodes[i];
    let Some(row) = node.row else {
        return Line::default();
    };
    let mut spans = Vec::new();
    if node.depth == 0 {
        spans.push(busy.unwrap_or_else(|| Span::styled("◆", theme::accent())));
        spans.push(Span::raw(" "));
    } else {
        spans.extend(branch_spans(nodes, i));
        let glyph = match (waiting(row), busy) {
            (Some(w), _) => Span::styled("⚑", waiting_style(w)),
            (None, Some(busy)) => busy,
            (None, None) if RESTING.contains(&row.status.as_str()) => {
                Span::styled("○", theme::dim())
            }
            (None, None) => Span::styled("●", theme::ok()),
        };
        spans.push(glyph);
        spans.push(Span::raw(" "));
    }
    let label_style = if hovered {
        theme::text().add_modifier(Modifier::UNDERLINED)
    } else {
        theme::text()
    };
    // A label too long for its status column is cut, keeping a space before the status.
    let room = column.saturating_sub(prefix_width(node) + 1);
    let label = if width(&row.label) > room {
        truncate(&row.label, room)
    } else {
        row.label.clone()
    };
    let used = prefix_width(node) + width(&label);
    spans.push(Span::styled(label, label_style));
    spans.push(Span::raw(" ".repeat(column.saturating_sub(used).max(1))));
    let status_style = match waiting(row) {
        Some(w) => waiting_style(w),
        None if row.agent == AgentRef::Quartermaster && row.status == "ready" => theme::ok(),
        None => theme::dim(),
    };
    spans.push(Span::styled(row.status.clone(), status_style));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::{agent_lines, nodes};
    use crate::app::App;
    use crate::sample;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use qm_types::api::{AgentRef, CrewRow, CrewTask};
    use qm_types::ids::{SecondmateId, TaskId, TaskLabelId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    fn crewmate(n: u32) -> AgentRef {
        AgentRef::Crewmate {
            task: TaskId::new(n).unwrap(),
        }
    }

    fn under(mut row: CrewRow, n: u32, words: &str) -> CrewRow {
        row.task = Some(CrewTask::new(TaskLabelId::new(n).unwrap(), words));
        row
    }

    /// The sample Bridge with nothing in a turn, so no line holds the moving busy indicator.
    fn app() -> App {
        let mut app = App::new();
        sample::install(&mut app);
        for row in &mut app.snapshot.crew {
            row.busy = false;
        }
        app
    }

    /// The Crew section's rows under its heading, up to the blank row that ends it.
    fn crew_box(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.after_draw();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let top = lines
            .iter()
            .position(|l| l.starts_with(" Crew "))
            .expect("the crew section is drawn");
        lines[top + 1..]
            .iter()
            .map(|l| {
                let inside: String = l.chars().skip(1).take(29).collect();
                inside.trim_end().to_owned()
            })
            .take_while(|l| !l.is_empty())
            .collect()
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_event(
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            Instant::now(),
        );
    }

    #[test]
    fn crewmates_sit_under_their_task_beneath_each_supervisor() {
        let mut app = app();
        assert_eq!(
            crew_box(&mut app, 150, 45),
            [
                "◆ Quartermaster     ready",
                "├ ◇ T-0042 api-server",
                "│ ├ ● C-0042 fix     12m",
                "│ └ ● C-0046 review  2m",
                "├ ◇ T-0039 api-server",
                "│ └ ⚑ C-0039 feat    approve",
                "├ ◇ T-0047 web-app",
                "│ └ ⚑ C-0047 fix     decide",
                "└ ○ S-02 web-app  idle",
                "  ├ ◇ T-0045 web-app",
                "  │ └ ● C-0045 build  3m",
                "  └ ◇ T-0048 dotfiles",
                "    └ ○ C-0048 fix    waits",
            ]
        );
    }

    #[test]
    fn a_task_gets_a_heading_under_each_supervisor_and_rows_without_one_stay_flat() {
        let qm = CrewRow::new(AgentRef::Quartermaster, 0, "Quartermaster", "ready");
        let sm = AgentRef::Secondmate {
            id: SecondmateId::new(2).unwrap(),
        };
        let crew = vec![
            qm,
            under(
                CrewRow::new(crewmate(1), 1, "C-0001 fix", "1m"),
                7,
                "T-0007 app",
            ),
            under(
                CrewRow::new(crewmate(2), 1, "C-0002 review", "2m"),
                7,
                "T-0007 app",
            ),
            // From a server that doesn't group: drawn where its row says.
            CrewRow::new(crewmate(3), 1, "C-0003 build", "3m"),
            CrewRow::new(sm, 1, "S-02 web", "idle"),
            // The same task under the secondmate gets a heading of its own there.
            under(
                CrewRow::new(crewmate(4), 2, "C-0004 fix", "4m"),
                7,
                "T-0007 app",
            ),
        ];
        let tree = nodes(&crew);
        let drawn: Vec<(u8, &str, bool)> = tree
            .iter()
            .map(|n| (n.depth, n.label, n.row.is_some()))
            .collect();
        assert_eq!(
            drawn,
            [
                (0, "Quartermaster", true),
                (1, "T-0007 app", false),
                (2, "C-0001 fix", true),
                (2, "C-0002 review", true),
                (1, "C-0003 build", true),
                (1, "S-02 web", true),
                (2, "T-0007 app", false),
                (3, "C-0004 fix", true),
            ]
        );
        assert_eq!(
            agent_lines(&crew),
            [
                (0, AgentRef::Quartermaster),
                (2, crewmate(1)),
                (3, crewmate(2)),
                (4, crewmate(3)),
                (5, sm),
                (7, crewmate(4)),
            ]
        );
    }

    #[test]
    fn the_keys_move_between_agents_passing_over_task_headings() {
        let mut app = app();
        crew_box(&mut app, 150, 45);
        app.select_crew(AgentRef::Quartermaster);
        let mut seen = Vec::new();
        for _ in 0..9 {
            key(&mut app, KeyCode::Down);
            seen.push(app.view.selected_crew.expect("a row stays selected"));
        }
        let sm = AgentRef::Secondmate {
            id: SecondmateId::new(2).unwrap(),
        };
        assert_eq!(
            seen,
            [
                crewmate(42),
                crewmate(46),
                crewmate(39),
                crewmate(47),
                sm,
                crewmate(45),
                crewmate(48),
                crewmate(48),
                crewmate(48),
            ]
        );
        key(&mut app, KeyCode::Up);
        assert_eq!(app.view.selected_crew, Some(crewmate(45)));
        key(&mut app, KeyCode::Home);
        assert_eq!(app.view.selected_crew, Some(AgentRef::Quartermaster));
    }

    #[test]
    fn a_deep_label_is_cut_only_as_far_as_its_own_status_needs() {
        let mut app = app();
        // S-02's C-0045 decides; a sibling under it has a long status.
        let sm_task = app.snapshot.crew[6].task.clone();
        app.snapshot.crew[6].status = "decide".to_owned();
        let mut sibling = app.snapshot.crew[6].clone();
        sibling.agent = crewmate(49);
        sibling.label = "C-0049 review".to_owned();
        sibling.status = "reported".to_owned();
        sibling.task = sm_task;
        app.snapshot.crew.insert(7, sibling);
        let lines = crew_box(&mut app, 150, 45);
        assert_eq!(lines[10], "  │ ├ ● C-0045 build decide");
        assert_eq!(lines[11], "  │ └ ○ C-0049 revi… reported");
    }

    #[test]
    fn a_long_heading_is_cut_to_the_pane_and_keeps_no_status_column() {
        let mut app = app();
        // C-0042 and C-0046, both in T-0042.
        for row in &mut app.snapshot.crew[1..3] {
            let task = row.task.as_mut().expect("in a task");
            "T-0042 a-project-with-a-very-long-name".clone_into(&mut task.label);
        }
        let lines = crew_box(&mut app, 150, 45);
        assert_eq!(lines[1], "├ ◇ T-0042 a-project-with-a-…");
        // The crewmates' statuses still follow their own labels.
        assert_eq!(lines[2], "│ ├ ● C-0042 fix     12m");
    }
}
