//! The projects pane: `●` accent when active, `○` dim otherwise, the name, then the delivery mode
//! and autonomy dim, aligned on the longest name (in a narrow pane the autonomy goes first, then
//! the mode, so the name stays readable). Beneath each project, a tree of its open tasks:
//! `├ T-0042 fix(auth): token refresh race`, the title dim. Right-click (or `Space`) a project for
//! its menu, which sets its autonomy (normal or auto); a task's line acts for its project.

use crate::app::App;
use crate::bridge::{ListRow, draw_list, empty_lines, heading_style, section_block};
use crate::hit::{FrameMap, Hit, Pane};
use crate::text::{truncate, width};
use crate::theme;
use qm_types::api::{ProjectRow, ProjectTask};
use qm_types::domain::DeliveryMode;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Draws the projects section into `area`.
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, map: &mut FrameMap) {
    let heading = Span::styled("Projects", heading_style(app, Pane::Projects));
    let block = section_block(vec![heading], true);
    let rows = rows(app, block.inner(area).width);
    draw_list(
        frame,
        area,
        block,
        rows,
        app.view.projects_scroll,
        Pane::Projects,
        map,
    );
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
    let name_width = projects.iter().map(|p| width(&p.name)).max().unwrap_or(0);
    let mut out = Vec::new();
    for (i, p) in projects.iter().enumerate() {
        let hit = Hit::Project(i);
        let selected = (app.view.selected_project == Some(i)).then_some(!app.view.pointer_picked);
        let hovered = app.term.hover == Some(hit);
        out.push(ListRow {
            line: line(p, name_width, inner_width, hovered),
            hit: Some(hit),
            selected,
        });
        for (j, task) in p.tasks.iter().enumerate() {
            out.push(ListRow {
                line: task_line(task, j + 1 == p.tasks.len(), inner_width),
                hit: Some(Hit::ProjectTask(i)),
                selected: None,
            });
        }
    }
    out
}

/// A task beneath its project: `├ T-0042 fix(auth): token refresh race` (`└` on the last), the
/// title dim and cut with `…` to fit `inner_width`.
fn task_line(task: &ProjectTask, last: bool, inner_width: u16) -> Line<'static> {
    let id = task.id.to_string();
    // The branch and its space, the ID, then a space before the title.
    let room = usize::from(inner_width).saturating_sub(width(&id) + 3);
    let mut spans = vec![
        Span::styled(if last { "└" } else { "├" }, theme::dim()),
        Span::raw(" "),
        Span::styled(id, theme::text()),
    ];
    if room > 0 && !task.title.trim().is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            truncate(task.title.trim(), room),
            theme::dim(),
        ));
    }
    Line::from(spans)
}

/// The fewest cells a project's name keeps before its row drops status words to make room.
const NAME_MIN: usize = 8;

/// One project: `● api-server  PR · normal` (or `branch · auto`, `in place · normal`).
/// When the row is too narrow, the name keeps at least [`NAME_MIN`] cells (or all of it, when
/// shorter): the autonomy goes first, then the mode, and only then is the name cut with `…`.
fn line(project: &ProjectRow, name_width: usize, inner_width: u16, hovered: bool) -> Line<'static> {
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
    let inner = usize::from(inner_width);
    let keep = width(&project.name).min(NAME_MIN);
    // The dot and its space, then two cells before the status.
    let (status, room) = [format!("{mode} · {}", project.autonomy), mode.to_owned()]
        .into_iter()
        .map(|status| {
            let room = inner.saturating_sub(width(&status) + 4);
            (status, room)
        })
        .find(|&(_, room)| room >= keep)
        .unwrap_or_else(|| (String::new(), inner.saturating_sub(2)));
    let name = truncate(&project.name, room.max(1));
    let pad = name_width.min(room).saturating_sub(width(&name)) + 2;
    let name_style = if hovered {
        theme::text().add_modifier(Modifier::UNDERLINED)
    } else {
        theme::text()
    };
    let mut spans = vec![
        Span::styled(dot, dot_style),
        Span::raw(" "),
        Span::styled(name, name_style),
    ];
    if !status.is_empty() {
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

    #[test]
    fn each_project_lists_its_open_tasks_beneath_it() {
        let mut app = App::new();
        sample::install(&mut app);
        let rows: Vec<String> = super::rows(&app, 29)
            .iter()
            .map(|row| row.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            rows,
            [
                "● api-server  PR · normal",
                "├ T-0042 fix(auth): token re…",
                "└ T-0039 feat(billing): pror…",
                "● web-app     PR · auto",
                "├ T-0045 feat(upload): resum…",
                "└ T-0047 fix(ci): flaky uplo…",
                "○ dotfiles    branch · normal",
                "└ T-0048 fix(zsh): slow prom…",
                "○ personal    branch · normal",
            ]
        );
        // The section grows into the rows Actions doesn't need, so the whole tree shows.
        assert_eq!(projects_box(&mut app, 150, 45), rows);
    }

    #[test]
    fn a_narrow_row_drops_its_status_words_before_it_cuts_the_name() {
        use qm_types::domain::{Autonomy, DeliveryMode};
        let project =
            qm_types::api::ProjectRow::new("api-server", DeliveryMode::Folder, Autonomy::Normal);
        let at = |inner: u16| -> String {
            let line = super::line(&project, 10, inner, false);
            assert!(line.width() <= usize::from(inner), "{inner}: {line}");
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        };
        assert_eq!(at(29), "○ api-server  folder · normal");
        // The name keeps eight cells before the autonomy goes...
        assert_eq!(at(27), "○ api-ser…  folder · normal");
        assert_eq!(at(26), "○ api-server  folder");
        // ...then the mode, and only then is the name cut.
        assert_eq!(at(18), "○ api-ser…  folder");
        assert_eq!(at(17), "○ api-server");
        assert_eq!(at(9), "○ api-se…");
    }

    #[test]
    fn a_task_line_acts_for_its_project() {
        let mut app = App::new();
        sample::install(&mut app);
        projects_box(&mut app, 150, 45);
        press(&mut app, Hit::ProjectTask(1), MouseButton::Left);
        assert_eq!(app.view.selected_project, Some(1));
        assert_eq!(app.focus, Pane::Projects);
        // No hint about adding a project: the click landed on a row.
        assert!(app.popup().is_none(), "{:?}", app.popup());
        // A right-click opens the project's own menu.
        projects_box(&mut app, 150, 45);
        press(&mut app, Hit::ProjectTask(0), MouseButton::Right);
        assert_eq!(app.view.selected_project, Some(0));
        assert!(app.view.menu.is_some());
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
