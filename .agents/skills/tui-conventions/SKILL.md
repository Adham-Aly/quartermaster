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
| `blue()` | `LightBlue` | a supervisor's marker in a session |
| `violet()` | `Magenta` | a supervisor's name |
| `filled()` | bright white on bright black | rest buttons, the current tab, the selected row |
| `button(true)` | black on yellow | a hovered button |
| `button_on_selection()` | black on bright white | a button on a selected row |
| `cursor()` | black on yellow | the cursor's cell in the prompt field |
| `selected()` | reversed | selected text in the prompt field |

Accent and warn are the same yellow; the words tell them apart. On the selected row, dim and
default text become bright white (`on_selection`). The app's own text selection is `REVERSED`.
Map core `Tone`s with `theme::tone`.

## Layout

- Rows: top bar, then the setup banner (Bridge only, when something is missing), the body, the
  status bar. Below 60×16 a too-small notice shows.
- Bridge columns: left 31 (20–60), a one-cell gap, the middle (never under 36), a gap, the task
  column 36 (24–70; `LEFT_DEFAULT`, `RIGHT_DEFAULT` in `app.rs`). Below 120 body columns the layout **folds**: no task column; the task pane opens as
  an overlay from the right edge with a ` ✕ `. Widths dragged on the dividers are saved to
  `<data dir>/tui.toml` (silently ignored if unreadable).
- Boxes: rounded borders, the title in the top border, one cell of padding inside, no gap rows
  between boxes, boxes at the frame's edges. Border dim; accent when focused; text colour when
  hovered. The input box is accent while focused or holding text.
- Buttons: `widgets/button.rs` (`" label "`), one space apart. Hints go on the status bar's right.
- Long labels are cut with `…` so status columns always fit; long problem text wraps instead.

## Input

- Every control has a key and every key is listed in Help (`placeholder.rs`), grouped by where it
  works. Globals: `Ctrl`+`C` twice quits (in a prompt field holding text it clears it first), `Ctrl`+`Z` suspends, `?` Help, `,` Settings, `Tab`/`⇧Tab`
  move focus. `Esc` does the first that applies: close the menu, the overlay, the selection,
  guidance mode, interrupt a running Quartermaster turn, then back to the Bridge (Help returns to
  the screen it was opened from). Letters are commands in list panes; they don't jump to the input.
- Mouse: widgets record clickable regions (`Hit`) and scroll areas in the `FrameMap` while
  drawing; input reads geometry only from the `FrameMap`, never recomputes it. Hover never moves
  focus. A click leaves no shading once the pointer moves on: after a mouse press, selected rows
  (projects, crew, Settings) keep their selection but drop the fill (`View::pointer_picked`)
  until a list key (`Tab`, arrows, Settings keys) shows it again. Focus borders always show,
  since letters act in the focused pane. The wheel scrolls the pane under the pointer (3 lines a
  notch). A double-click is a second press on the same `Hit` within 400 ms. Right-click (or
  `Space`) opens a row's context menu with its first item on the pointer's row. Record
  overlapping regions so the topmost is recorded last.
- Selection and copy: widgets push `TextRow`s for selectable text; a drag selects, release copies
  through `pbcopy` (OSC 52 over SSH or as a fallback) and flashes `Copied`.
- The prompt field (`widgets/input_box.rs`, layout in `input/field.rs`) is shared by the Bridge
  and every session. It soft-wraps, grows from 2 to 10 rows then scrolls, and over 30 wrapped
  rows shows the first 4, `...[truncated N lines]...`, the last 5 and 2 rows around the cursor;
  the whole text is kept and sent. Its text, cursor and selection are one plain `Draft`
  (`input/draft.rs`, byte offsets), apart from the per-window gesture state in `Pointer`: a drag or `Shift`+movement selects, release copies, typing replaces, delete keys
  remove it. `Ctrl`+`V` is left free for image paste; `⌘V` arrives as a bracketed paste.

## Text and terminal

- Streaming text shows whole lines only (`widgets/stream.rs::committed`), so nothing reflows.
- Markdown in messages goes through `widgets/markdown.rs`.
- `terminal.rs` enters raw mode, the alternate screen, mouse capture with motion (SGR), bracketed
  paste, and synchronized updates per frame; it restores the terminal on drop, panic and suspend.
- `qm-tui` doesn't log and never blocks on the core.

## Verify

`cargo xtask check`, `cargo build`, then look at the screen in a real terminal and with
`tmux capture-pane -e` at every size above, including the matching `QM_DEV_SAMPLE` screen.
