//! The projects section: one line per project, `● api-server     PR · 2 tasks`. The dot is teal
//! while work runs there and a dim `○` otherwise; at the right edge, how its work is delivered
//! (`PR`, `branch`, `in place`, `folder`), `auto` when it runs on auto, and how many tasks are
//! open there. When the line is narrow those words go whole (the delivery, then `auto`, then the
//! count) before the name is cut.
//!
//! Enter, or a click on a project, opens its details under it (another closes it; `Esc` too):
//!
//! ```text
//! ● api-server     PR · 2 tasks
//!   ~/code/api-server
//!   PR to main · normal
//!   ⎈ Token refresh race    12m
//!   ⚑ Proration preview approve
//! ```
//!
//! Its tasks read as they do in Crew (`crew::task_mark`). Right-click (or `Space`) a project for
//! its menu, which sets its autonomy (normal or auto); a detail line acts for its project.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, heading_style, section_block};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width};
use crate::theme;
use crate::widgets::crew;
use qm_types::api::{ProjectRow, ProjectTask};
use qm_types::domain::{Autonomy, DeliveryMode};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::ops::Range;

/// Draws the projects section into `area`. When they don't all fit, the heading counts them and
/// the last line in view says how many are below: `↓ 4 more, all quiet`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let probe = section_block(Vec::new(), true).inner(area);
    let mut rows = rows(app, probe.width);
    let visible = usize::from(probe.height);
    let overflows = visible > 0 && rows.len() > visible;
    let mut heading = vec![Span::styled("Projects", heading_style(app, Pane::Projects))];
    if overflows {
        let count = app.snapshot.projects.len();
        heading.push(Span::styled(format!(" · {count}"), theme::dim()));
    }
    let max = rows.len().saturating_sub(visible);
    let scroll = app.view.projects_scroll.min(max);
    if overflows && scroll < max {
        let at = scroll + visible - 1;
        rows[at] = ListRow::plain(more_line(app, at));
    }
    let block = section_block(heading, true);
    draw_list(frame, area, block, rows, scroll, Pane::Projects, map);
}

/// The line standing in for everything from line `from` down: `↓ 4 more, all quiet` when none
/// of them is busy, `↓ 4 more` when some are, `↓ more` when only details are left.
fn more_line(app: &App, from: usize) -> Line<'static> {
    let below: Vec<&ProjectRow> = app
        .snapshot
        .projects
        .iter()
        .enumerate()
        .filter(|(i, _)| lines_of(app, *i).start >= from)
        .map(|(_, p)| p)
        .collect();
    let words = match below.len() {
        0 => "↓ more".to_owned(),
        n if below.iter().all(|p| !p.active) => format!("↓ {n} more, all quiet"),
        n => format!("↓ {n} more"),
    };
    Line::from(vec![Span::raw("  "), Span::styled(words, theme::dim())])
}

/// Whether `project`'s details show.
fn is_open(app: &App, project: &ProjectRow) -> bool {
    app.view.open_project.as_deref() == Some(project.name.as_str())
}

/// The lines project `i` takes in the section: its own, then its details while open.
pub(crate) fn lines_of(app: &App, i: usize) -> Range<usize> {
    let size = |p: &ProjectRow| {
        1 + if is_open(app, p) {
            2 + p.tasks.len()
        } else {
            0
        }
    };
    let projects = &app.snapshot.projects;
    let start: usize = projects.iter().take(i).map(size).sum();
    start..start + projects.get(i).map_or(1, size)
}

impl App {
    /// Opens project `i`'s details, or closes them when they show.
    pub(crate) fn toggle_project(&mut self, i: usize) {
        let Some(name) = self.snapshot.projects.get(i).map(|p| p.name.clone()) else {
            return;
        };
        let open = (self.view.open_project.as_ref() != Some(&name)).then_some(name);
        self.change(|app| &mut app.view.open_project, open);
        let lines = lines_of(self, i);
        self.keep_row_visible(Pane::Projects, lines.end - 1);
        self.keep_row_visible(Pane::Projects, lines.start);
    }
}

/// The pane's rows at `inner_width`; their count sizes the box.
pub(crate) fn rows(app: &App, inner_width: u16) -> Vec<ListRow> {
    let projects = &app.snapshot.projects;
    if projects.is_empty() {
        return empty_lines(
            &[
                "No projects yet.",
                "Projects appear when work starts in a folder.",
            ],
            inner_width,
        )
        .into_iter()
        .map(ListRow::plain)
        .collect();
    }
    let inner = usize::from(inner_width);
    let mut out = Vec::new();
    for (i, p) in projects.iter().enumerate() {
        let hit = Hit::Project(i);
        let selected = (app.view.selected_project == Some(i)).then_some(!app.view.pointer_picked);
        let hovered = app.term.hover == Some(hit);
        out.push(ListRow {
            line: line(p, inner, hovered),
            hit: Some(hit),
            selected,
        });
        if is_open(app, p) {
            let detail = |line| ListRow {
                line,
                hit: Some(Hit::ProjectTask(i)),
                selected: None,
            };
            let place = if p.place.is_empty() {
                &p.name
            } else {
                &p.place
            };
            out.push(detail(dim_line(place, inner)));
            out.push(detail(dim_line(&settings_words(p), inner)));
            for task in &p.tasks {
                out.push(detail(task_line(app, task, inner)));
            }
        }
    }
    out
}

/// A dim detail line, indented under its project's name.
fn dim_line(text: &str, inner: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(truncate(text, inner.saturating_sub(2)), theme::dim()),
    ])
}

/// `PR to main · normal`, `branch off main · auto`, `in place on main · normal`,
/// `plain folder · normal`.
fn settings_words(project: &ProjectRow) -> String {
    let base = project.base_branch.as_str();
    let delivery = match (project.delivery, base.is_empty()) {
        (DeliveryMode::DirectPr, false) => format!("PR to {base}"),
        (DeliveryMode::DirectPr, true) => "PR".to_owned(),
        (DeliveryMode::BranchOnly, false) => format!("branch off {base}"),
        (DeliveryMode::BranchOnly, true) => "branch".to_owned(),
        (DeliveryMode::BuildInPlace, false) => format!("in place on {base}"),
        (DeliveryMode::BuildInPlace, true) => "in place".to_owned(),
        (DeliveryMode::Folder, _) => "plain folder".to_owned(),
    };
    format!("{delivery} · {}", project.autonomy)
}

/// One of a project's tasks, as Crew shows it: `⎈ Token refresh race    12m`. A task Crew doesn't
/// list (yet) shows its title, dim.
fn task_line(app: &App, task: &ProjectTask, inner: usize) -> Line<'static> {
    let (glyph, name, status, status_style) = crew::task_mark(app, task.id).unwrap_or_else(|| {
        (
            Span::styled("○", theme::dim()),
            task.title.trim().to_owned(),
            String::new(),
            theme::dim(),
        )
    });
    let room = inner.saturating_sub(4);
    let status = if width(&status) + 1 + NAME_MIN.min(width(&name)) <= room {
        status
    } else {
        String::new()
    };
    let gap = usize::from(!status.is_empty());
    let name = truncate(&name, room.saturating_sub(width(&status) + gap));
    let pad = room.saturating_sub(width(&name) + width(&status));
    let mut spans = vec![Span::raw("  "), glyph, Span::raw(" ")];
    spans.push(Span::styled(name, theme::text()));
    if !status.is_empty() {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(status, status_style));
    }
    Line::from(spans)
}

/// The fewest cells a project's name keeps before its line drops words to make room.
const NAME_MIN: usize = 16;

/// One project: `● api-server     PR · 2 tasks`, the words at the right edge.
fn line(project: &ProjectRow, inner: usize, hovered: bool) -> Line<'static> {
    let (dot, dot_style) = if project.active {
        ("●", theme::accent())
    } else {
        ("○", theme::dim())
    };
    let mode = match project.delivery {
        DeliveryMode::DirectPr => "PR",
        DeliveryMode::BranchOnly => "branch",
        DeliveryMode::BuildInPlace => "in place",
        DeliveryMode::Folder => "folder",
    };
    let count = match project.tasks.len() {
        0 => String::new(),
        1 => "1 task".to_owned(),
        n => format!("{n} tasks"),
    };
    // Each word with the order it goes in when there's no room: the delivery first.
    let mut words: Vec<(u8, String)> = vec![(0, mode.to_owned())];
    if project.autonomy == Autonomy::Auto {
        words.push((1, "auto".to_owned()));
    }
    words.push((2, count));
    words.retain(|(_, w)| !w.is_empty());
    let joined = |words: &[(u8, String)]| {
        words
            .iter()
            .map(|(_, w)| w.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let keep = width(&project.name).min(NAME_MIN);
    // The dot and its space, then at least two cells before the words.
    let room = |words: &[(u8, String)]| {
        let status = joined(words);
        inner.saturating_sub(
            2 + if status.is_empty() {
                0
            } else {
                width(&status) + 2
            },
        )
    };
    for drop in 0..3 {
        if room(&words) >= keep {
            break;
        }
        words.retain(|(rank, _)| *rank != drop);
    }
    let status = joined(&words);
    let name = truncate(&project.name, room(&words).max(1));
    let name_style = if hovered {
        theme::text().add_modifier(Modifier::UNDERLINED)
    } else {
        theme::text()
    };
    let mut spans = vec![
        Span::styled(dot, dot_style),
        Span::raw(" "),
        Span::styled(name.clone(), name_style),
    ];
    if !status.is_empty() {
        let pad = inner.saturating_sub(2 + width(&name) + width(&status));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(status, theme::dim()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::hit::{Hit, Pane};
    use crate::sample;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    /// The Projects section's rows under its heading, up to the blank row that ends it.
    fn projects_box(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.after_draw();
        let buffer = terminal.backend().buffer();
        let lines: Vec<String> = (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let top = lines
            .iter()
            .position(|l| l.starts_with(" Projects "))
            .expect("the projects section is drawn");
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

    fn press(app: &mut App, hit: Hit, button: MouseButton) {
        let area = app.term.drawn.hits.rect_of(hit).expect("drawn");
        for kind in [MouseEventKind::Down(button), MouseEventKind::Up(button)] {
            let mouse = MouseEvent {
                kind,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            };
            app.on_event(Event::Mouse(mouse), Instant::now());
        }
    }

    fn texts(rows: &[crate::bridge::ListRow]) -> Vec<String> {
        rows.iter()
            .map(|row| row.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn each_project_is_one_line_and_its_tasks_live_in_crew() {
        let mut app = App::new();
        sample::install(&mut app);
        let rows = texts(&super::rows(&app, 29));
        assert_eq!(
            rows,
            [
                "● api-server     PR · 3 tasks",
                "● web-app      auto · 2 tasks",
                "○ dotfiles    branch · 1 task",
                "○ notes                folder",
            ]
        );
        assert_eq!(projects_box(&mut app, 150, 45), rows);
    }

    #[test]
    fn enter_opens_a_project_s_details_under_it_and_esc_closes_them() {
        let mut app = App::new();
        sample::install(&mut app);
        for row in &mut app.snapshot.crew {
            row.busy = false;
        }
        app.focus = Pane::Projects;
        // Nothing is picked yet: Enter opens the first.
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.view.selected_project, Some(0));
        assert_eq!(
            texts(&super::rows(&app, 29)),
            [
                "● api-server     PR · 3 tasks",
                "  ~/code/api-server",
                "  PR to main · normal",
                "  ● Token refresh race 2 crew",
                "  ⚑ Proration preview approve",
                "  ✓ API docs refresh reported",
                "● web-app      auto · 2 tasks",
                "○ dotfiles    branch · 1 task",
                "○ notes                folder",
            ]
        );
        assert_eq!(super::lines_of(&app, 1), 6..7);
        // Another project's opens in its place.
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let rows = texts(&super::rows(&app, 29));
        assert_eq!(
            rows[3..],
            [
                "○ notes                folder",
                "  ~/Documents/notes",
                "  plain folder · normal"
            ]
        );
        key(&mut app, KeyCode::Esc);
        assert_eq!(super::rows(&app, 29).len(), 4);
    }

    #[test]
    fn a_narrow_line_drops_its_words_whole_before_it_cuts_the_name() {
        use qm_types::domain::{Autonomy, DeliveryMode};
        let mut project =
            qm_types::api::ProjectRow::new("api-server", DeliveryMode::DirectPr, Autonomy::Auto);
        project.tasks = vec![qm_types::api::ProjectTask::new(
            qm_types::ids::TaskLabelId::new(1).unwrap(),
            "x",
        )];
        let at = |inner: usize| -> String {
            let line = super::line(&project, inner, false);
            assert!(line.width() <= inner, "{inner}: {line}");
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        };
        assert_eq!(at(34), "○ api-server    PR · auto · 1 task");
        // The delivery goes first, then auto, then the count...
        assert_eq!(at(29), "○ api-server    auto · 1 task");
        assert_eq!(at(26), "○ api-server        1 task");
        assert_eq!(at(18), "○ api-server");
        // ...and only then is the name cut.
        assert_eq!(at(9), "○ api-se…");
    }

    #[test]
    fn a_task_line_acts_for_its_project() {
        let mut app = App::new();
        sample::install(&mut app);
        app.view.open_project = Some("web-app".to_owned());
        projects_box(&mut app, 150, 45);
        press(&mut app, Hit::ProjectTask(1), MouseButton::Left);
        assert_eq!(app.view.selected_project, Some(1));
        assert_eq!(app.focus, Pane::Projects);
        // No hint about adding a project: the click landed on a row.
        assert!(app.popup().is_none(), "{:?}", app.popup());
        // A right-click opens the project's own menu.
        app.view.open_project = Some("api-server".to_owned());
        projects_box(&mut app, 150, 45);
        press(&mut app, Hit::ProjectTask(0), MouseButton::Right);
        assert_eq!(app.view.selected_project, Some(0));
        assert!(app.view.menu.is_some());
    }

    #[test]
    fn a_long_list_counts_its_projects_and_says_how_many_more_are_below() {
        use qm_types::api::ProjectRow;
        use qm_types::domain::{Autonomy, DeliveryMode};
        let mut app = App::new();
        sample::install(&mut app);
        for n in 0..6 {
            let name = format!("extra-{n}");
            app.snapshot.projects.push(ProjectRow::new(
                name,
                DeliveryMode::Folder,
                Autonomy::Normal,
            ));
        }
        app.focus = Pane::Projects;
        let lines = projects_box(&mut app, 100, 30);
        let last = lines.last().unwrap().trim_start();
        assert!(
            last.starts_with("↓ ") && last.ends_with(" more, all quiet"),
            "{lines:#?}"
        );
        let more: usize = last[4..].split(' ').next().unwrap().parse().unwrap();
        // Every project is either a line in view or counted below.
        assert_eq!(lines.len() - 1 + more, 10, "{lines:#?}");
        // Moving to the last one scrolls it into view, and the hint goes.
        for _ in 0..10 {
            key(&mut app, KeyCode::Down);
            projects_box(&mut app, 100, 30);
        }
        let lines = projects_box(&mut app, 100, 30);
        assert_eq!(lines.last().unwrap(), "○ extra-5              folder");
        assert!(!lines.iter().any(|l| l.contains('↓')), "{lines:#?}");
    }

    #[test]
    fn the_keys_move_between_projects_and_keep_the_selected_one_in_view() {
        let mut app = App::new();
        sample::install(&mut app);
        app.focus = Pane::Projects;
        // Short enough that the Projects section scrolls.
        projects_box(&mut app, 100, 30);
        for _ in 0..3 {
            app.on_event(
                Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                Instant::now(),
            );
            projects_box(&mut app, 100, 30);
        }
        assert_eq!(app.view.selected_project, Some(2));
        let lines = projects_box(&mut app, 100, 30);
        assert!(
            lines.iter().any(|l| l.starts_with("○ dotfiles")),
            "{lines:#?}"
        );
    }
}
