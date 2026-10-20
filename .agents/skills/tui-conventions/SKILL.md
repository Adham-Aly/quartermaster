---
name: tui-conventions
description: How qm-tui screens are drawn and driven - the 16-colour palette roles in theme.rs, boxes, buttons, the Bridge geometry and fold, keys and mouse (hit map, hover, wheel, context menus, the app's own selection and copy), streaming text and terminal modes. Use when drawing or changing any screen, widget, key or mouse behaviour in the terminal UI, or when judging whether a screen looks right.
---

# Terminal UI conventions

Be picky: compare every screen cell by cell at 150×45, 120×40, 119×40 and 100×30, in a light and
a dark terminal theme, and fix what looks off (skill `run-by-hand`).

## Colour: the terminal's 16 only

Never an RGB or 256-colour value. Use the roles in `crates/qm-tui/src/theme.rs`, never a `Color`
directly in a widget. `Backend16` (`backend.rs`) sends 16-colour SGR codes (`30`–`37`, `90`–`97`,
`40`–`47`, `100`–`107`); crossterm alone would send `38;5;n`.

| Role | Style | Used for |
|---|---|---|
| text | default fg | body text |
| `strong()` | default fg, bold | names (`you`, `quartermaster`), headings |
| `dim()` | `DarkGray` (bright black) | secondary text, idle borders, hints |
| `accent()` | `Yellow` | focus, the active input, approvals |
| `warn()` | `Yellow` | decisions, notices, the setup banner, tier headings |
| `ok()` | `Green` | running, ready, passed |
| `error()` | `Red` | errors |
| `badge()` | `Red` | the dot on a menu entry and the menu button: something there needs you |
| `blue()` | `LightBlue` | a supervisor's marker in a session |
| `violet()` | `Magenta` | a supervisor's name |
| `filled()` | bright white on bright black | rest buttons (the menu button too), the current Settings tab, the selected row (keyboard) |
| `button(true)` | black on yellow | a hovered button |
| `button_on_selection()` | black on bright white | a button on a selected row |
| `cursor()` | black on yellow | the cursor's cell in the prompt field |
| `selected()` | reversed | selected text in the prompt field |

Accent and warn are the same yellow; the words tell them apart. On the selected row, dim and
default text become bright white (`on_selection`). The app's own text selection is `REVERSED`.
Map core `Tone`s with `theme::tone`.

## Layout

- Rows: top bar (the ` ☰ Menu ` button at the left, this Mac and the accounts at the right), then
  the setup banner (Bridge only, when something is missing), the body, the
  status bar. Below 60×16 a too-small notice shows.
- Bridge columns: left 31 (20–60), a one-cell gap, the middle (never under 36), a gap, the task
  column 36 (24–70; `LEFT_DEFAULT`, `RIGHT_DEFAULT` in `app.rs`). Below 120 body columns the layout **folds**: no task column; the task pane opens as
  an overlay from the right edge with a ` ✕ `. Widths dragged on the dividers are saved to
  `<data dir>/tui.toml` (silently ignored if unreadable).
- Boxes: rounded borders, the title in the top border, one cell of padding inside, no gap rows
  between boxes, boxes at the frame's edges. Border dim; accent when focused; text colour when
  hovered. The input box is accent while focused or holding text.
- Popups: Settings and Help aren't screens; they open over the current screen (`popups.rs`) from
  the menu or `,` / `?`, and leave its focus alone, so closing (`Esc`, ` ✕ `, a click outside)
  goes back where you were. The window is `widgets/popup.rs` (`centered` places it, `draw`
  records backdrop, window and close hits and returns the inside); lists are
  `widgets/dropdown.rs` (`Item` with a key hint and a badge), used by context menus and the menu
  (`MenuTarget::Main`). Open popups are a stack (`View::popups`, top last; `Esc` closes the top)
  of serialisable `OpenPopup`s. Popups and menus store an `Anchor` (centre, menu button, a row
  plus an offset), never screen cells, resolved and clamped each frame (`popups::resolve`). A new
  popup or menu entry: see the notes at the top of `popups.rs`. A badge: make
  `App::entry_badge` true for the entry; the menu button follows.
- Buttons: `widgets/button.rs` (`" label "`), one space apart. Hints go on the status bar's right.
- Long labels are cut with `…` so status columns always fit; long problem text wraps instead.

## Input

- Every control has a key and every key is listed in Help (`placeholder.rs`), grouped by where it
  works. Globals: `Ctrl`+`C` twice quits (in a prompt field holding text it clears it first),
  `Ctrl`+`Z` suspends, `Ctrl`+`O` the menu (even while typing), `?` Help, `,` Settings,
  `Tab`/`⇧Tab` move focus. An open popup takes every other key. `Esc` does the first that
  applies: close the menu (or the slash-command menu), the popup, the overlay, the selection,
  guidance mode, interrupt a running Quartermaster turn, then back to the Bridge. Letters are
  commands in list panes; they don't jump to the input.
- Slash commands: `/` at the start of your message to the Quartermaster (Bridge only) opens the
  command menu above the input box (`input/slash.rs`, `widgets/slash_menu.rs`). A new command is
  one `COMMANDS` entry, a `qm_types::api::SlashCommand` variant and its arm in `qm-core`
  `turns/slash.rs`.
- Mouse: widgets record clickable regions (`Hit`) and scroll areas in the `FrameMap` while
  drawing; input reads geometry only from the `FrameMap`, never recomputes it. Hover never moves
  focus. A click leaves no shading once the pointer moves on: after a mouse press, a selected
  row (projects, crew, Settings) trades its fill for an accent `▸` in its left padding
  (`View::pointer_picked`, drawn only by `widgets/selection.rs`) until a list key (`Tab`,
  arrows, Settings keys) brings the fill back. Focus borders always show, since letters act in
  the focused pane. The wheel scrolls the pane under the pointer (3 lines a notch). A
  double-click is a second press on the same `Hit` within 400 ms. Right-click (or `Space`) opens
  a row's context menu with its first item on the pointer's row. Record overlapping regions so
  the topmost is recorded last.
- Selection and copy: widgets push `TextRow`s for selectable text; a drag selects, release copies
  through `pbcopy` (OSC 52 over SSH or as a fallback) and flashes `Copied`.
- The prompt field (`widgets/input_box.rs`, layout in `input/field.rs`) is shared by the Bridge
  and every session. It soft-wraps, grows from 2 to 10 rows then scrolls, and over 30 wrapped
  rows shows the first 4, `...[truncated N lines]...`, the last 5 and 2 rows around the cursor;
  the whole text is kept and sent. Its text, cursor, selection and pasted images are one plain
  `Draft` (`input/draft.rs`, byte offsets), apart from the per-window gesture state in `Pointer`:
  a drag or `Shift`+movement selects, release copies, typing replaces, delete keys remove it.
- Paste: `⌘V` arrives as a bracketed paste, text only (`on_paste`). `Ctrl`+`V` reads the
  pasteboard's images off the UI thread (`input/clipboard_image.rs`), inserts `[Image #N]` (deleted
  as one piece) and keeps each image in the `Draft` until it goes with `Command::Send { images }`.

## Text and terminal

- Streaming text shows whole lines only (`widgets/stream.rs::committed`), so nothing reflows.
- An agent thinking or busy shows the one busy indicator, `widgets/spinner.rs` (`⣾ thinking`, or
  the glyph alone in the crew tree), never a widget's own. It redraws only while drawn: one cell,
  8 steps a second, nothing when none is on screen.
- Markdown in messages goes through `widgets/markdown.rs`.
- Your own prompts sit in a dim rounded box (`conversation::prompt_box`), the header in its top
  border, on the Bridge and in sessions. A run of tool calls in a session is one dim line,
  `▸ 12 tool calls…` (the `…` while it is the newest thing in a running turn); a click or `e`
  shows each call under `▾`. Which runs are expanded is shared UI state (`View::tool_groups`),
  keyed by agent and the run's first line ID, never by screen position.
- `terminal.rs` enters raw mode, the alternate screen, mouse capture with motion (SGR), bracketed
  paste, and synchronized updates per frame; it restores the terminal on drop, panic and suspend.
- `qm-tui` doesn't log and never blocks on the core.

## Verify

`cargo xtask check`, `cargo build`, then look at the screen in a real terminal and with
`tmux capture-pane -e` at every size above, including the matching `QM_DEV_SAMPLE` screen.
