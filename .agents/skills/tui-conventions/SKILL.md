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

On entering full-screen mode (and on every resume), `theme::detect` asks the terminal for its
colours (OSC 10, 11 and 4 for all 16, ended by a DA1 query so a silent terminal costs no wait;
`COLORFGBG` as the fallback; `theme/query.rs`). `theme/fit.rs` then gives each role the first of
its colours that reads on that background (4.5:1, else 3:1, else the default text colour): a light
theme whose yellow fades gets blue, a faint bright black gets a darker grey, and so on. The
`Palette` lives on the window's state (`App::palette`) and `App::draw` puts it in force for the
frame (`theme::scoped`); never cache a `Style` across frames, cache the role function instead. The table is the palette when the
terminal says nothing; it is tuned for dark themes. Check a change in a light and a dark theme with
tmux's `window-style` and `pane-colours[n]`, which tmux reports as the terminal's colours.

| Role | Style (dark default) | Used for |
|---|---|---|
| text | default fg | body text |
| `strong()` | default fg, bold | names (`you`, `quartermaster`), headings |
| `dim()` | `DarkGray` (bright black) | secondary text, idle borders, hints |
| `accent()` | `Yellow` | focus, the active input, approvals |
| `warn()` | `Yellow` | decisions, notices, tier headings |
| `ok()` | `Green` | running, ready, passed |
| `error()` | `Red` | errors |
| `badge()` | `Red` | the dot on a menu entry, a popup's title and beside the menu button (never on a fill or the accent): something there needs you |
| `blue()` | `LightBlue` | a supervisor's marker in a session |
| `violet()` | `Magenta` | a supervisor's name |
| `filled()` | bright white on bright black | rest buttons (the menu button too), the current Settings tab, the selected row (keyboard) |
| `button(true)` | black on yellow | a hovered button |
| `button_on_selection()` | black on bright white | a button on a selected row |
| `cursor()` | black on yellow | the cursor's cell in the prompt field |
| `selected()` | reversed | selected text in the prompt field |

Accent and warn are the same colour; the words tell them apart. On the selected row, dim and
default text take the fill's text colour, and a coloured glyph that can't be read on the fill
takes its other brightness (`on_selection`). The app's own text selection is `REVERSED`.
Map core `Tone`s with `theme::tone`.

## Layout

- Rows: top bar (the ` ☰ Menu ` button at the left, then the core's update notice when it has
  one and there's room, this Mac and the accounts at the right), the body, the status bar.
  Below 60×16 a too-small notice shows.
- Bridge columns: left 31 (20–60), a one-cell gap, the middle (never under 36), a gap, the task
  column 36 (24–70; `LEFT_DEFAULT`, `RIGHT_DEFAULT` in `app.rs`). Below 120 body columns the layout **folds**: no task column; the task pane opens as
  an overlay from the right edge with a ` ✕ `. Widths dragged on the dividers are saved
  with the rest of the shared UI state in `<data dir>/ui.json` (`persist.rs`: debounced 1 s, atomic,
  0600, saved on detach and stop; loading never fails; never logged). New shared state worth
  keeping across a server restart goes into `persist.rs` `State` and `Restored`.
- Boxes: rounded borders, the title in the top border, one cell of padding inside, no gap rows
  between boxes, boxes at the frame's edges. Border dim; accent when focused; text colour when
  hovered. The input box is accent while focused or holding text.
- Popups: Settings, Help and Update (`update.rs`) aren't screens; they open over the current
  screen (`popups.rs`) from the menu or `,` / `?` (Update also from its top-bar notice), and leave its focus alone, so closing (`Esc`, ` ✕ `, a click outside)
  goes back where you were. The window is `widgets/popup.rs` (`centered` places it, `draw`
  records backdrop, window and close hits and returns the inside); lists are
  `widgets/dropdown.rs` (`Item` with a key hint and a badge), used by context menus and the menu
  (`MenuTarget::Main`). Open popups are a stack (`View::popups`, top last; `Esc` closes the top)
  of serialisable `OpenPopup`s. Popups and menus store an `Anchor` (centre, menu button, the
  Projects pane, a row plus an offset), never screen cells, resolved and clamped each frame
  (`popups::resolve`). A click on the Projects box outside its rows (or `a` there) opens the
  projects hint under its title; `Esc` or any click closes it. A new popup or menu entry: see
  the notes at the top of `popups.rs`. A badge: make `App::entry_badge` true for the entry; the
  menu button follows (Update's comes from the core, `UpdateView::badge`).
- Settings' nested pickers, text fields and Connect view have a visible `← Back`, equivalent
  to `Esc`: dismiss the picker/field without saving, or return from Connect to Accounts (not cancel
  a running login). The Back row stays visible while content scrolls. Settings uses 48% of the
  body's width and height (minimum 72×19, maximum width 149, clamped to the terminal). Overflow
  shows a scrollbar with directional arrows; each arrow disappears at its end. Pickers show
  `↑ more` / `↓ more` on their borders. Routing's Task kinds use two aligned grid columns when
  whole entries fit, one otherwise, ordered down each column to match ↑/↓. Conditional build/fix
  labels are written in full. Default notes stay hidden; changed/empty notes sit beside the tier
  when they fit, otherwise wrap within the cell. Its `?` button (or `?` on Routing) opens
  task-kind explanations over Settings; wheel/arrows/page keys scroll, `Esc`, `✕` or an outside
  click closes just that help window, keeping Routing's selection and scroll position.
- Settings ▸ Model levels shows placed **model + effort** labels, not an effort toggle.
  **Add model…** (`a` or `/`) opens a fixed searchable catalogue of all models × their
  supported efforts (one `(no effort)` entry with `None` when support is empty). Search
  includes name, ID, account and effort; adding never consumes a catalogue entry. Choosing
  a combination asks for its destination tier, excluding identical placements. Moves and
  drags also exclude duplicate destinations; `x` removes. There is no add-to-tier context
  submenu. Use `PlaceModel { from: None, effort, to, .. }` for additions. Routing's separate
  Quartermaster effort control overrides the entry effort; with model and effort both Auto,
  the selected entry's effort is used. Board selections and pickers are transient, not
  persisted in `ui.json`.
- Diff (`diff.rs`): a changed-path-only tree with aggregate folder status badges (`A/M/D/R/C`),
  then the selected file's Before/After panes with independent line-number gutters. Consecutive
  deletion/addition runs are paired; unmatched sides stay empty. Tab switches tree/code focus,
  brackets select files, arrows/pages/Home/End move or scroll, and left/right pan code in cells.
  The wheel scrolls only the pane under the pointer. Binary, empty and mode-only files have
  explicit notices; partial patches warn above the panes. Card approval keys and Back are unchanged.
- Buttons: `widgets/button.rs` (`" label "`), one space apart. Hints go on the status bar's right.
- Long labels are cut with `…` so status columns always fit; long problem text wraps instead.

## Input

- Every control has a key and every key is listed in Help (`placeholder.rs`), grouped by where it
  works. Globals: `Ctrl`+`C` twice quits (in a prompt field holding text it clears it first),
  `Ctrl`+`Z` suspends, `Ctrl`+`O` the menu (even while typing), `?` Help, `,` Settings,
  `Tab`/`⇧Tab` move focus. An open popup takes every other key. `Esc` does the first that
  applies: close the menu (or the slash-command menu), Settings' nested view, the popup, the overlay, the selection,
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
  8 steps a second, nothing when none is on screen. An age or running seconds count sets
  `App::ages_shown` when drawn, so ticks redraw once a second while one is on screen.
- The context strip (`widgets/context_strip.rs`) sits between the middle column's box and the
  input: 1–3 dim rows (one under 24 rows, two under 34) for the shown agent's context against its
  auto-compact point, and its current or last turn. It drops parts rather than truncating, and
  says "unknown" or "stale" rather than show a 0. Compaction notes sit in the chat by time.
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
