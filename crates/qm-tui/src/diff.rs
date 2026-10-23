//! The Diff screen.
//!
//! Opened by an approval card's Diff button (or its key): the UI sends
//! `Command::ShowDiff { held: Some(id) }` and the core answers with `Snapshot.diff`. Not a tab.
//!
//! ```text
//! Diff · T-0039 · feat(billing): proration preview · +214 −37 · 12 files            ← Back
//! ╭──────────────────────────────────────────────────────────────────────────────────────╮
//! │ diff --git a/src/billing.rs b/src/billing.rs                                (accent) │
//! │ +    let preview = prorate(plan, at);                                        (ok)    │
//! ╰──────────────────────────────────────────────────────────────────────────────────────╯
//!  ⚑ 1 approval · 4 running                                   esc back · a approve · r reject
//! ```
//!
//! The lines scroll with the wheel, `↑`/`↓`, `PgUp`/`PgDn` and `g`/`G`; the app's selection and
//! copy work as everywhere. The card's keys (`a`, `r`, `1`…) answer the card from here and return
//! to the Bridge; `Esc` and `← Back` close the screen.

use crate::app::App;
use crate::hit::{FrameMap, Hit, TextRow};
use crate::router::Screen;
use crate::text::{cells, truncate, width};
use crate::theme;
use crossterm::event::{KeyCode, KeyEvent};
use qm_types::api::{ActionItem, AgentRef, Command};
use qm_types::ids::HeldActionId;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use std::time::Instant;

/// Something on the Diff screen a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffHit {
    /// `← Back`.
    Back,
    /// The box of lines: the wheel scrolls it.
    Body,
}

/// The `← Back` button's label.
const BACK: &str = "← Back";
/// The keys, on the status bar's right.
pub(crate) const HINTS: &str = "esc back · a approve · r reject";
/// Shown while the core reads the diff.
const LOADING: &str = "Reading the diff…";
/// The wheel's step, in lines.
const WHEEL_LINES: usize = 3;

impl App {
    /// Opens the Diff screen for `held` (the approval card's Diff button).
    pub(crate) fn show_diff(&mut self, held: Option<HeldActionId>, now: Instant) {
        let Some(held) = held else {
            self.flash(crate::input::NOT_BUILT, now);
            return;
        };
        self.view.diff_scroll = 0;
        self.command(Command::ShowDiff { held: Some(held) });
        self.show(Screen::Diff);
    }

    /// Closes the Diff screen and returns to the Bridge, with the Actions overlay its Diff button
    /// came from open again while that item still waits.
    pub(crate) fn close_diff(&mut self) {
        self.view.diff_scroll = 0;
        self.command(Command::ShowDiff { held: None });
        self.show(Screen::Bridge);
        let from = self.view.diff_from.take();
        if let Some(index) = from.and_then(|key| {
            self.snapshot
                .actions
                .iter()
                .position(|item| item.key == key)
        }) {
            self.open_action(index);
        }
    }

    /// How far the diff can scroll, from the last frame.
    fn diff_scroll_max(&self) -> usize {
        let rows = self
            .term
            .drawn
            .hits
            .rect_of(Hit::Diff(DiffHit::Body))
            .map_or(0, |r| usize::from(r.height.saturating_sub(2)));
        let lines = self.snapshot.diff.as_ref().map_or(0, |d| d.lines.len());
        lines.saturating_sub(rows)
    }

    fn diff_page(&self) -> usize {
        self.term
            .drawn
            .hits
            .rect_of(Hit::Diff(DiffHit::Body))
            .map_or(1, |r| usize::from(r.height.saturating_sub(3)).max(1))
    }

    fn scroll_diff_to(&mut self, to: usize) {
        let to = to.min(self.diff_scroll_max());
        self.change(|app| &mut app.view.diff_scroll, to);
    }

    /// The Actions item of the diff's held action, while it waits.
    fn diff_item(&self) -> Option<&ActionItem> {
        let held = self.snapshot.diff.as_ref()?.held;
        self.snapshot
            .actions
            .iter()
            .find(|item| item.card.held == Some(held))
    }

    /// A key on the Diff screen; `false` when it isn't the screen's (the global keys follow).
    pub(crate) fn on_diff_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        let current = self.view.diff_scroll;
        match key.code {
            KeyCode::Esc => self.close_diff(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_diff_to(current.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_diff_to(current + 1),
            KeyCode::PageUp => self.scroll_diff_to(current.saturating_sub(self.diff_page())),
            KeyCode::PageDown => self.scroll_diff_to(current + self.diff_page()),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_diff_to(0),
            KeyCode::End | KeyCode::Char('G') => self.scroll_diff_to(usize::MAX),
            KeyCode::Char(c) => {
                // The card's keys answer it from here, and return to the Bridge without
                // reopening its overlay.
                let Some(item) = self.diff_item() else {
                    return false;
                };
                if item.card.answered.is_some() {
                    return false;
                }
                let Some(action) = item
                    .card
                    .buttons
                    .iter()
                    .find(|b| b.key.eq_ignore_ascii_case(&c))
                    .map(|b| b.action.clone())
                else {
                    return false;
                };
                let held = item.card.held;
                let reply_to = item.from.unwrap_or(AgentRef::Quartermaster);
                self.view.diff_from = None;
                self.close_diff();
                self.run_card_action(action, held, None, reply_to, now);
            }
            _ => return false,
        }
        true
    }

    /// The wheel over the Diff screen.
    pub(crate) fn diff_wheel(&mut self, up: bool) {
        let current = self.view.diff_scroll;
        let to = if up {
            current.saturating_sub(WHEEL_LINES)
        } else {
            current + WHEEL_LINES
        };
        self.scroll_diff_to(to);
    }

    /// A click on the Diff screen.
    pub(crate) fn diff_click(&mut self, hit: DiffHit) {
        match hit {
            DiffHit::Back => self.close_diff(),
            DiffHit::Body => {}
        }
    }
}

/// Draws the Diff screen into `body`.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    if body.height < 4 || body.width < 10 {
        return;
    }
    let view = app.snapshot.diff.as_ref();

    // The header: "Diff · title · stat", and ← Back on the right.
    let back_width = cells(width(BACK)) + 2;
    let header = Rect::new(body.x, body.y, body.width, 1);
    let back = Rect::new(
        body.right().saturating_sub(back_width + 1),
        body.y,
        back_width,
        1,
    );
    let mut title = vec![Span::styled(" Diff", theme::accent())];
    if let Some(view) = view {
        title.push(Span::styled(" · ", theme::dim()));
        let room = usize::from(header.width.saturating_sub(back_width + 8));
        let text = if view.stat.is_empty() {
            view.title.clone()
        } else {
            format!("{} · {}", view.title, view.stat)
        };
        let note = if view.truncated {
            " · cut at 20,000 lines"
        } else {
            ""
        };
        let text = truncate(&text, room.saturating_sub(cells(width(note)).into()));
        title.push(Span::styled(text, theme::text()));
        title.push(Span::styled(note, theme::warn()));
    }
    frame.render_widget(Line::from(title), header);
    let hovered = app.term.hover == Some(Hit::Diff(DiffHit::Back));
    frame.render_widget(
        Paragraph::new(Span::styled(format!(" {BACK} "), theme::button(hovered))),
        back,
    );
    map.hits.add(back, Hit::Diff(DiffHit::Back));

    // The box of lines.
    let area = Rect::new(body.x, body.y + 1, body.width, body.height - 1);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::border(true, false));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    map.hits.add(area, Hit::Diff(DiffHit::Body));
    let rows = usize::from(inner.height);
    let lines: Vec<(String, ratatui::style::Style)> = match view {
        Some(view) => view
            .lines
            .iter()
            .map(|s| (s.text.clone(), theme::tone(s.tone)))
            .collect(),
        None => vec![(LOADING.to_owned(), theme::dim())],
    };
    let max = lines.len().saturating_sub(rows);
    let scroll = app.view.diff_scroll.min(max);
    let room = usize::from(inner.width.saturating_sub(1));
    for (i, (text, style)) in lines.iter().skip(scroll).take(rows).enumerate() {
        let y = inner.y + cells(i);
        let shown = truncate(&text.replace('\t', "    "), room);
        let row = Rect::new(inner.x + 1, y, inner.width.saturating_sub(1), 1);
        frame.render_widget(Line::from(Span::styled(shown.clone(), *style)), row);
        map.text.push(TextRow {
            at: Position::new(row.x, y),
            text: shown,
            continues: false,
        });
    }
}
