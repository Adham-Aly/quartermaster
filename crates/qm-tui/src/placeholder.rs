//! Help, shown in a popup: every key and what the mouse does, grouped by where they apply. It
//! scrolls (wheel, arrows, `PgUp`/`PgDn`, `g`/`G`), and descriptions wrap under their column.
//! (Settings lives in `settings`.)

use crate::app::App;
use crate::hit::{FrameMap, Pane};
use crate::text::{width, wrap};
use crate::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Cells between the key column and the descriptions.
const GAP: usize = 3;

/// Draws Help into `inner`, the inside of its popup, scrolled by `app.view.help_scroll`
/// (clamped), and records its scroll area for the wheel.
pub(crate) fn draw_help(frame: &mut Frame<'_>, inner: Rect, app: &App, map: &mut FrameMap) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines = lines(usize::from(inner.width));
    let max = lines.len().saturating_sub(usize::from(inner.height));
    let scroll = app.view.help_scroll.min(max);
    let shown: Vec<Line<'static>> = lines
        .into_iter()
        .skip(scroll)
        .take(usize::from(inner.height))
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
    map.scroll_areas.push((inner, Pane::Help));
    map.scroll_max.push((Pane::Help, max));
}

/// Help's rows at `room` cells: each group's heading, then its keys, a blank row between groups;
/// a description too long for its line wraps under the description column.
pub(crate) fn lines(room: usize) -> Vec<Line<'static>> {
    let key_width = HELP
        .iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|(k, _)| width(k))
        .max()
        .unwrap_or(0);
    let lead = 2 + key_width + GAP;
    let text_room = room.saturating_sub(lead).max(12);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, (group, keys)) in HELP.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        out.push(Line::styled(*group, theme::accent()));
        for (key, what) in *keys {
            for (j, row) in wrap(what, text_room).into_iter().enumerate() {
                let head = if j == 0 {
                    vec![
                        Span::raw("  "),
                        Span::styled(*key, theme::strong()),
                        Span::raw(" ".repeat(key_width - width(key) + GAP)),
                    ]
                } else {
                    vec![Span::raw(" ".repeat(lead))]
                };
                let mut spans = head;
                spans.push(Span::styled(row, theme::dim()));
                out.push(Line::from(spans));
            }
        }
    }
    out
}

/// The keys and the mouse, grouped by where they apply, and the Settings, setup-check, session
/// and Diff screens' own keys.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Anywhere",
        &[
            (
                "ctrl+c twice",
                "quit; while you are typing, the first clears your message",
            ),
            ("ctrl+z", "suspend to the shell; fg comes back"),
            ("tab · shift+tab", "next or previous pane"),
            (
                "ctrl+o",
                "the menu: settings, update, help (even while typing)",
            ),
            (
                "esc",
                "close the menu, popup or overlay, clear the selection, leave guidance, interrupt the Quartermaster, back",
            ),
            (
                "?",
                "this help (when not typing); esc goes back where you were",
            ),
            (",", "settings (when not typing)"),
        ],
    ),
    (
        "Menu",
        &[
            ("↑ ↓ · k j", "move between entries"),
            ("⏎", "open the entry in a popup"),
            ("esc", "close the menu"),
        ],
    ),
    (
        "Projects and crew",
        &[
            ("↑ ↓ · k j", "move the selection"),
            ("home end · g G", "first or last row"),
            ("⏎ · o", "open the selected crewmate"),
            ("space", "the selected row's menu"),
            ("a", "projects: where they come from"),
        ],
    ),
    (
        "Actions",
        &[
            ("↑ ↓ · k j", "move the selection; nothing is answered here"),
            ("⏎ · o · click", "open the item's detail"),
            ("space", "the selected item's menu"),
            ("in the detail: ← →", "pick a button; ⏎ presses it"),
            (
                "in the detail: a d r · 1 2 3",
                "press the button with that letter",
            ),
        ],
    ),
    (
        "Conversation",
        &[
            ("↑ ↓ · k j", "scroll a line"),
            ("pgup pgdn", "scroll a page"),
            ("home end · g G", "oldest or newest"),
        ],
    ),
    (
        "Task",
        &[
            ("↑ ↓ · pgup pgdn", "scroll"),
            ("⏎", "open session"),
            ("s", "stop"),
            ("r", "open completed report in browser"),
        ],
    ),
    (
        "Worker session",
        &[
            (
                "⏎ · o · double-click",
                "open the selected crewmate's session in the middle",
            ),
            (
                "tab · shift+tab",
                "the panes, the session in the conversation's place",
            ),
            ("⏎", "send to the worker directly"),
            ("i", "interrupt its running turn"),
            (
                "e",
                "expand or collapse the newest detail or tool calls in view",
            ),
            ("↑ ↓ · pgup pgdn · g G", "scroll the session"),
            (
                "esc · ← Quartermaster",
                "back to the Quartermaster (or click its crew row)",
            ),
        ],
    ),
    (
        "Diff",
        &[
            ("d", "in an approval's detail: open its diff"),
            ("tab · shift+tab", "focus the changed-file tree or code"),
            ("[ ] · click file", "previous or next changed file"),
            (
                "↑ ↓ · pgup pgdn · g G",
                "move through files, or scroll code",
            ),
            ("← →", "pan long code lines, keeping both gutters visible"),
            ("a · r", "approve or reject from the diff"),
            ("esc · ← Back", "back to the Bridge, the detail open again"),
        ],
    ),
    (
        "Settings",
        &[
            ("tab · ← → · 1 to 5", "the next, previous or a numbered tab"),
            ("↑ ↓ · home end", "move the selection"),
            ("pgup pgdn", "scroll a tab taller than the screen"),
            (
                "esc · ← Back",
                "dismiss a picker or field, or return from Connect; otherwise close Settings",
            ),
            (
                "⏎ · space",
                "the selected row's menu, or what its control does",
            ),
            (
                "c p * l s d r",
                "accounts: connect, pause, preferred, label, soft limit, disconnect, reconnect",
            ),
            (
                "a / m x",
                "model levels: add/search model + effort, move, remove",
            ),
            (
                "space c f q e",
                "routing: the pin, its model, its fallback, the Quartermaster's model, its effort",
            ),
            ("− +", "routing: autocompact window, for every agent"),
            (
                "?",
                "routing: explain task kinds (Esc or ✕ closes the popup)",
            ),
            (
                "a · space",
                "projects: flip the selected project between normal and auto",
            ),
            ("n · space", "system: turn notifications on or off"),
            ("s", "system: the setup check"),
        ],
    ),
    (
        "Setup check",
        &[
            ("↑ ↓", "move between the tools that need you"),
            ("⏎", "run the selected tool's first step in a pane"),
            ("c", "copy its command"),
            ("esc", "back (cancels a running pane)"),
        ],
    ),
    (
        "Your message",
        &[
            ("⏎", "send"),
            ("⌥⏎ · shift+⏎ · ctrl+j", "new line"),
            (
                "ctrl+v",
                "paste an image from the clipboard as [Image #1]; ⌘V pastes text",
            ),
            ("← → · ⌥← ⌥→", "move by a character or a word"),
            ("↑ ↓", "move a row, wrapped rows included"),
            ("home end · ctrl+a ctrl+e", "start or end of the line"),
            ("shift + any of these", "select as you move"),
            (
                "⌫ · delete",
                "delete a character back or forward, or the selection",
            ),
            ("⌥⌫ · ctrl+w", "delete a word back"),
            ("ctrl+u · ctrl+k", "delete to the line's start or end"),
            ("typing or pasting", "replaces the selection"),
            ("ctrl+c", "clear your message"),
            ("drag · shift+click", "select (a drag copies it)"),
            (
                "over 30 rows",
                "shows its start, ...[truncated N lines]... and its end; all of it is sent",
            ),
        ],
    ),
    (
        "Slash commands (the Quartermaster's chat)",
        &[
            ("/", "at the start of your message: the command menu"),
            (
                "/clear",
                "clear the Quartermaster's context and start a new session",
            ),
            (
                "/compact",
                "compact the Quartermaster's context to free up room",
            ),
            ("↑ ↓ · tab", "pick a command, or complete its name"),
            ("⏎ · click", "run it"),
            ("esc", "close the menu"),
        ],
    ),
    (
        "Mouse",
        &[
            (
                "click",
                "focus a pane, select a row, press a button, switch tabs, open the menu",
            ),
            ("click the Projects box", "where projects come from"),
            ("click outside · ✕", "close the popup"),
            ("double-click", "open a crewmate"),
            ("right-click", "a row's menu (a project's: normal or auto)"),
            ("drag a border", "resize the columns beside it"),
            ("drag over text", "select and copy it"),
            ("wheel", "scroll the pane under the pointer"),
        ],
    ),
];
