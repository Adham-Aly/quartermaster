//! The setup check screen.
//!
//! What this Mac has of `git`, the GitHub CLI and Claude Code comes from the core in
//! `Snapshot.setup`, display-ready. The screen never opens by itself: the core checks in the
//! background, and what needs you shows as a red dot until it's fixed; you open the screen from
//! Settings.
//!
//! Nothing runs without your click. A Run button starts its command in an embedded terminal pane
//! (`pane`), where you see exactly what Homebrew or a login does and can type into it; `Esc` or
//! Cancel stops it. When it exits the app asks the core to check again, and the pane shows what
//! came of it with the next step's button ("Log in to GitHub"). Without Homebrew the check shows
//! the official commands to copy, and checks again when you come back to the window.
//!
//! - `check` builds the rows (pure).
//! - `pane` is the pseudo-terminal and its drawing.

mod check;
pub(crate) mod pane;

use crate::app::App;
use crate::bridge::pane_block;
use crate::hit::{FrameMap, Hit};
use crate::router::Screen;
use crate::text::cells;
use crate::theme;
use check::Row;
use crossterm::event::{KeyCode, KeyEvent};
use pane::Pane;
use qm_types::api::{Command, SetupActionKind, SetupItem, SetupMark, SetupTool};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

/// After this long without any input on the setup screen, the next input counts as you coming
/// back to the window, and the check runs again.
const AWAY_AFTER: Duration = Duration::from_secs(5);

/// Something on the setup screen a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetupHit {
    /// The Bridge's one-line banner.
    Banner,
    /// Button `action` of item `item` (indexes into `SetupView::items` and its actions).
    Action { item: usize, action: usize },
    /// The Copy button of a numbered step.
    CopyStep { item: usize, step: usize },
    /// The next step's button under a finished pane.
    Next { run: usize },
    /// Done, in the "Setup check" box beside the panes.
    Done,
    /// Cancel on a running pane.
    Cancel { run: usize },
    /// A pane's screen: a click focuses it.
    Pane { run: usize },
}

/// One install or login, running or finished.
#[derive(Debug)]
pub(crate) struct Run {
    tool: SetupTool,
    /// "brew install gh", the pane's title.
    title: String,
    argv: Vec<String>,
    verb: String,
    pane: Pane,
    /// Exited, and the core was asked to check again when the check had run this many times:
    /// the result waits for a higher count.
    awaiting: Option<u64>,
    /// Its next step was pressed: what it showed then stays, so the finished install shows beside
    /// the login it led to.
    followed: Option<(Option<Line<'static>>, NextStep)>,
}

impl Run {
    fn is_install(&self) -> bool {
        self.verb == "install"
    }
}

/// The setup screen's state, kept by the app (the panes own processes, so it lives beside the
/// view, not in it).
#[derive(Debug, Default)]
pub(crate) struct SetupState {
    runs: Vec<Run>,
    /// The item the keys act on, by tool so it survives new snapshots.
    selected: Option<SetupTool>,
    /// The last terminal event while the setup screen showed.
    last_event: Option<Instant>,
}

impl SetupState {
    fn running(&self) -> Option<usize> {
        self.runs.iter().rposition(|r| r.pane.running())
    }
}

/// What the result under a finished pane says, and its next step.
struct Outcome {
    line: Option<Line<'static>>,
    next: Option<NextStep>,
}

#[derive(Clone, Debug)]
struct NextStep {
    label: String,
    title: String,
    argv: Vec<String>,
    verb: String,
}

fn short_name(tool: SetupTool) -> &'static str {
    match tool {
        SetupTool::Git => "git",
        SetupTool::Gh => "gh",
        _ => "Claude Code",
    }
}

impl App {
    fn setup_item(&self, tool: SetupTool) -> Option<&SetupItem> {
        self.snapshot.setup.items.iter().find(|i| i.tool == tool)
    }

    /// The item the keys act on: the one you moved to while it still needs you, else the first
    /// that does.
    fn setup_current(&self) -> Option<usize> {
        let items = &self.snapshot.setup.items;
        let needs = |i: &SetupItem| i.mark != SetupMark::Ok;
        self.setup
            .selected
            .and_then(|tool| items.iter().position(|i| i.tool == tool && needs(i)))
            .or_else(|| items.iter().position(needs))
    }

    /// What came of a finished run, from the latest check.
    fn outcome(&self, run: &Run) -> Outcome {
        if let Some((line, next)) = &run.followed {
            return Outcome {
                line: line.clone(),
                next: Some(next.clone()),
            };
        }
        let retry = NextStep {
            label: "Try again".to_owned(),
            title: run.title.clone(),
            argv: run.argv.clone(),
            verb: run.verb.clone(),
        };
        let Some(exit) = run.pane.exit() else {
            return Outcome {
                line: None,
                next: None,
            };
        };
        if run.awaiting.is_some() {
            return Outcome {
                line: Some(Line::styled("Checking again…", theme::dim())),
                next: None,
            };
        }
        if exit.cancelled {
            return Outcome {
                line: Some(Line::styled("Cancelled.", theme::dim())),
                next: Some(retry),
            };
        }
        let item = self.setup_item(run.tool);
        let fixed_now = item.is_some_and(|i| match i.mark {
            SetupMark::Missing => false,
            SetupMark::Ok => true,
            _ => run.is_install(),
        });
        if !fixed_now {
            let line = if exit.success {
                Line::from(vec![
                    Span::styled("! ", theme::warn()),
                    Span::raw(format!("{} still needs you", short_name(run.tool))),
                ])
            } else {
                Line::from(vec![
                    Span::styled("✗ ", theme::error()),
                    Span::raw(format!("{} exited with code {}", run.title, exit.code)),
                ])
            };
            return Outcome {
                line: Some(line),
                next: Some(retry),
            };
        }
        let line = run.is_install().then(|| {
            let version = item.and_then(|i| i.version.clone()).unwrap_or_default();
            Line::from(vec![
                Span::styled("✓ ", theme::ok()),
                Span::raw(format!("{} {version} installed", short_name(run.tool))),
            ])
        });
        let next = item.filter(|i| i.mark != SetupMark::Ok).and_then(|i| {
            i.actions.iter().find_map(|a| match &a.kind {
                SetupActionKind::Run { title, argv, verb } if *title != run.title => {
                    Some(NextStep {
                        label: a.label.clone(),
                        title: title.clone(),
                        argv: argv.clone(),
                        verb: verb.clone(),
                    })
                }
                _ => None,
            })
        });
        Outcome { line, next }
    }

    /// Starts a command in a new pane. Only one runs at a time.
    fn start_run(&mut self, tool: SetupTool, step: NextStep, now: Instant) {
        if self.setup.running().is_some() {
            self.flash("Wait for the running step to finish, or cancel it.", now);
            return;
        }
        let (rows, cols) = self.setup.runs.last().map_or_else(
            || {
                (
                    self.term.size.1.saturating_sub(6),
                    (self.term.size.0 / 2).saturating_sub(4),
                )
            },
            |r| r.pane.size(),
        );
        match Pane::spawn(&step.argv, rows, cols) {
            Ok(pane) => {
                self.setup.runs.push(Run {
                    tool,
                    title: step.title,
                    argv: step.argv,
                    verb: step.verb,
                    pane,
                    awaiting: None,
                    followed: None,
                });
                self.redraw();
            }
            Err(why) => self.flash(why, now),
        }
    }

    /// Presses button `action` of item `item`.
    fn press_setup_action(&mut self, item: usize, action: usize, now: Instant) {
        let Some(it) = self.snapshot.setup.items.get(item) else {
            return;
        };
        let tool = it.tool;
        let Some(a) = it.actions.get(action) else {
            return;
        };
        let label = a.label.clone();
        match a.kind.clone() {
            SetupActionKind::Run { title, argv, verb } => self.start_run(
                tool,
                NextStep {
                    label,
                    title,
                    argv,
                    verb,
                },
                now,
            ),
            SetupActionKind::Copy { text } => self.copy(&text, now),
            SetupActionKind::Later => self.setup_later(),
            _ => self.flash(crate::input::NOT_BUILT, now),
        }
    }

    /// Not now / `Esc` on the check: closes it; the red dot stays until it's fixed.
    fn setup_later(&mut self) {
        self.show(Screen::Bridge);
    }

    /// Done beside the panes: back to the check, or to the Bridge when nothing needs you.
    fn setup_done(&mut self) {
        if self.setup.running().is_some() {
            return;
        }
        self.setup.runs.clear();
        let all_ok = self
            .snapshot
            .setup
            .items
            .iter()
            .all(|i| i.mark == SetupMark::Ok);
        if all_ok {
            self.show(Screen::Bridge);
        }
        self.redraw();
    }

    fn press_next(&mut self, run: usize, now: Instant) {
        let Some(r) = self.setup.runs.get(run) else {
            return;
        };
        if r.followed.is_some() {
            return;
        }
        let tool = r.tool;
        let Outcome { line, next } = self.outcome(r);
        let Some(next) = next else {
            return;
        };
        let before = self.setup.runs.len();
        self.start_run(tool, next.clone(), now);
        if self.setup.runs.len() > before {
            self.setup.runs[run].followed = Some((line, next));
        }
    }

    /// A click on the setup screen or the banner.
    pub(crate) fn setup_click(&mut self, hit: SetupHit, now: Instant) {
        match hit {
            SetupHit::Banner => self.show(Screen::Setup),
            SetupHit::Action { item, action } => {
                if let Some(tool) = self.snapshot.setup.items.get(item).map(|i| i.tool) {
                    self.setup.selected = Some(tool);
                }
                self.press_setup_action(item, action, now);
            }
            SetupHit::CopyStep { item, step } => {
                let text = self
                    .snapshot
                    .setup
                    .items
                    .get(item)
                    .and_then(|i| i.steps.get(step))
                    .map(|s| s.command.clone());
                if let Some(text) = text {
                    self.copy(&text, now);
                }
            }
            SetupHit::Next { run } => self.press_next(run, now),
            SetupHit::Done => self.setup_done(),
            SetupHit::Cancel { run } => {
                if let Some(r) = self.setup.runs.get_mut(run) {
                    r.pane.cancel();
                }
                self.redraw();
            }
            SetupHit::Pane { .. } => {}
        }
    }

    /// A key on the setup screen; `false` leaves it to the global keys.
    pub(crate) fn on_setup_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        if let Some(run) = self.setup.running() {
            if key.code == KeyCode::Esc {
                self.setup.runs[run].pane.cancel();
                self.redraw();
            } else {
                let bytes = pane::key_bytes(key);
                self.setup.runs[run].pane.write(&bytes);
            }
            return true;
        }
        if !self.setup.runs.is_empty() {
            match key.code {
                KeyCode::Esc => self.setup_done(),
                KeyCode::Enter => {
                    let last = self.setup.runs.len() - 1;
                    if self.outcome(&self.setup.runs[last]).next.is_some() {
                        self.press_next(last, now);
                    } else {
                        self.setup_done();
                    }
                }
                _ => return key.code != KeyCode::Char('?'),
            }
            return true;
        }
        let current = self.setup_current();
        match key.code {
            KeyCode::Esc => self.setup_later(),
            KeyCode::Up | KeyCode::Char('k') => self.step_setup_item(false),
            KeyCode::Down | KeyCode::Char('j') => self.step_setup_item(true),
            KeyCode::Enter => {
                if let Some(item) = current {
                    let default = check::default_action(&self.snapshot.setup.items[item]);
                    if let Some(action) = default {
                        self.press_setup_action(item, action, now);
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(item) = current {
                    let it = &self.snapshot.setup.items[item];
                    let text = it
                        .actions
                        .iter()
                        .find_map(|a| match &a.kind {
                            SetupActionKind::Copy { text } => Some(text.clone()),
                            _ => None,
                        })
                        .or_else(|| it.steps.first().map(|s| s.command.clone()));
                    if let Some(text) = text {
                        self.copy(&text, now);
                    }
                }
            }
            KeyCode::Char('?') => return false,
            _ => {}
        }
        true
    }

    /// Moves the keys' item to the previous or next one that needs you.
    fn step_setup_item(&mut self, forward: bool) {
        let needing: Vec<SetupTool> = self
            .snapshot
            .setup
            .items
            .iter()
            .filter(|i| i.mark != SetupMark::Ok)
            .map(|i| i.tool)
            .collect();
        let Some(current) = self
            .setup_current()
            .map(|i| self.snapshot.setup.items[i].tool)
        else {
            return;
        };
        let at = needing.iter().position(|t| *t == current).unwrap_or(0);
        let to = if forward {
            (at + 1).min(needing.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        if let Some(tool) = needing.get(to) {
            self.setup.selected = Some(*tool);
            self.redraw();
        }
    }

    /// Pasted text goes to the running pane.
    pub(crate) fn setup_paste(&mut self, text: &str) {
        if let Some(run) = self.setup.running() {
            self.setup.runs[run].pane.write(text.as_bytes());
        }
    }

    /// Any terminal event: the terminal regaining focus (`focus_gained`) while something needs
    /// you, or on the setup screen the first event after a while away, runs the check again, for
    /// the steps you did outside the app.
    pub(crate) fn setup_activity(&mut self, now: Instant, focus_gained: bool) {
        if self.screen != Screen::Setup {
            self.setup.last_event = None;
            if focus_gained && !self.snapshot.setup_issues.is_empty() {
                self.command(Command::RecheckSetup);
            }
            return;
        }
        let away = self
            .setup
            .last_event
            .is_some_and(|at| now.saturating_duration_since(at) >= AWAY_AFTER);
        self.setup.last_event = Some(now);
        if (away || focus_gained) && self.setup.running().is_none() {
            self.command(Command::RecheckSetup);
        }
    }

    /// Time-driven: pane output, panes fitting their boxes, and runs that exited (the check runs
    /// again for each).
    pub(crate) fn setup_tick(&mut self, _now: Instant) {
        let generation = self.snapshot.setup.generation;
        let mut recheck = false;
        let mut dirty = false;
        for (index, run) in self.setup.runs.iter_mut().enumerate() {
            if let Some(area) = self
                .term
                .drawn
                .hits
                .rect_of(Hit::Setup(SetupHit::Pane { run: index }))
                && run.pane.running()
            {
                run.pane.resize(area.height, area.width);
            }
            dirty |= run.pane.take_dirty();
            let was_running = run.pane.running();
            if was_running && run.pane.poll().is_some() {
                run.awaiting = Some(generation);
                recheck = true;
                dirty = true;
            }
        }
        if recheck {
            self.command(Command::RecheckSetup);
        }
        if dirty {
            self.redraw();
        }
    }

    /// A new snapshot: finished runs whose re-check came back show their result.
    pub(crate) fn setup_after_snapshot(&mut self) {
        let generation = self.snapshot.setup.generation;
        for run in &mut self.setup.runs {
            if run.awaiting.is_some_and(|g| generation > g) {
                run.awaiting = None;
            }
        }
    }
}

/// The status bar's left side on the setup screen: where Homebrew is.
pub(crate) fn status_left(app: &App) -> Option<String> {
    app.snapshot.setup.homebrew.clone()
}

/// The status bar's key hints on the setup screen.
pub(crate) fn hints(app: &App) -> String {
    if app.setup.running().is_some() {
        return "type into the pane · esc cancel".to_owned();
    }
    if !app.setup.runs.is_empty() {
        let next = app
            .setup
            .runs
            .last()
            .is_some_and(|r| app.outcome(r).next.is_some());
        return if next {
            "⏎ next step · esc done"
        } else {
            "⏎ done"
        }
        .to_owned();
    }
    let mut parts = Vec::new();
    if let Some(item) = app.setup_current().map(|i| &app.snapshot.setup.items[i]) {
        if let Some(SetupActionKind::Run { verb, .. }) =
            check::default_action(item).map(|a| &item.actions[a].kind)
        {
            parts.push(format!("⏎ {verb}"));
        }
        let copy = !item.steps.is_empty()
            || item
                .actions
                .iter()
                .any(|a| matches!(a.kind, SetupActionKind::Copy { .. }));
        if copy {
            parts.push("c copy".to_owned());
        }
    }
    parts.push("esc back".to_owned());
    parts.join(" · ")
}

fn hover_of(app: &App) -> Option<SetupHit> {
    match app.term.hover {
        Some(Hit::Setup(hit)) => Some(hit),
        _ => None,
    }
}

/// Draws rows into `inner`, recording their hits.
fn draw_rows(frame: &mut Frame<'_>, inner: Rect, rows: Vec<Row>, map: &mut FrameMap) {
    for (y, row) in (inner.y..inner.bottom()).zip(rows) {
        for (x, w, hit) in row.hits {
            map.hits.add(
                Rect::new(inner.x + x, y, w.min(inner.width.saturating_sub(x)), 1),
                Hit::Setup(hit),
            );
        }
        frame.render_widget(row.line, Rect::new(inner.x, y, inner.width, 1));
    }
}

/// Draws the setup screen into `body`.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    if app.setup.runs.is_empty() {
        draw_check(frame, body, app, map);
    } else {
        draw_runs(frame, body, app, map);
    }
}

/// The check: one box, "Setup check · studio".
fn draw_check(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    let title = if app.snapshot.machine.is_empty() {
        "Setup check".to_owned()
    } else {
        format!("Setup check · {}", app.snapshot.machine)
    };
    let block = pane_block(&title, theme::dim());
    let probe = block.inner(body);
    let rows = check::check_rows(
        &app.snapshot.setup,
        probe.width,
        app.setup_current(),
        hover_of(app),
    );
    let height = (cells(rows.len()) + 2).min(body.height);
    let area = Rect::new(body.x, body.y, body.width, height);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_rows(frame, inner, rows, map);
}

/// The panes: the earlier run on the left; the latest on the right above the "Setup check" box.
fn draw_runs(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    let left_width = body.width.saturating_sub(1) / 2;
    let left = Rect::new(body.x, body.y, left_width, body.height);
    let right = Rect::new(
        body.x + left_width + 1,
        body.y,
        body.width.saturating_sub(left_width + 1),
        body.height,
    );
    let mut tools: Vec<SetupTool> = Vec::new();
    for run in &app.setup.runs {
        if !tools.contains(&run.tool) {
            tools.push(run.tool);
        }
    }
    let summary = check::summary_rows(&app.snapshot.setup, &tools, hover_of(app));
    let summary_height = (cells(summary.len()) + 2).min(right.height);
    let n = app.setup.runs.len();
    let summary_area = if n == 1 {
        draw_run(frame, left, app, 0, map);
        Rect::new(right.x, right.y, right.width, summary_height)
    } else {
        draw_run(frame, left, app, n - 2, map);
        let top = Rect::new(
            right.x,
            right.y,
            right.width,
            right.height.saturating_sub(summary_height),
        );
        draw_run(frame, top, app, n - 1, map);
        Rect::new(right.x, top.bottom(), right.width, summary_height)
    };
    let block = pane_block("Setup check", theme::dim());
    let inner = block.inner(summary_area);
    frame.render_widget(block, summary_area);
    draw_rows(frame, inner, summary, map);
}

/// One pane: its command as the title (accent while it runs), its screen, and once it's finished
/// what came of it.
fn draw_run(frame: &mut Frame<'_>, area: Rect, app: &App, index: usize, map: &mut FrameMap) {
    let Some(run) = app.setup.runs.get(index) else {
        return;
    };
    let running = run.pane.running();
    // The latest pane has the keyboard (accent), running or not.
    let style = if index + 1 == app.setup.runs.len() {
        theme::accent()
    } else {
        theme::dim()
    };
    let block = pane_block(&run.title, style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if running {
        let label = "Cancel";
        let w = crate::widgets::button::width_of(label);
        if area.width > w + 4 {
            let at = Rect::new(area.right() - w - 2, area.y, w, 1);
            let hit = SetupHit::Cancel { run: index };
            frame.render_widget(
                crate::widgets::button::span(label, app.term.hover == Some(Hit::Setup(hit))),
                at,
            );
            map.hits.add(at, Hit::Setup(hit));
        }
        map.hits
            .add(inner, Hit::Setup(SetupHit::Pane { run: index }));
        let focused = app.screen == Screen::Setup;
        if let Some(cursor) = run.pane.draw(frame.buffer_mut(), inner, 0, focused) {
            frame.set_cursor_position(cursor);
        }
        return;
    }
    let outcome = app.outcome(run);
    let tail = check::result_rows(
        outcome.line,
        outcome.next.as_ref().map(|n| n.label.as_str()),
        index,
    );
    let room = inner.height.saturating_sub(cells(tail.len()));
    let used = run.pane.used_rows();
    let from = used.saturating_sub(room);
    let shown = used.min(room);
    run.pane.draw(
        frame.buffer_mut(),
        Rect::new(inner.x, inner.y, inner.width, shown),
        from,
        false,
    );
    let tail_area = Rect::new(
        inner.x,
        inner.y + shown,
        inner.width,
        inner.height.saturating_sub(shown),
    );
    draw_rows(frame, tail_area, tail, map);
}
