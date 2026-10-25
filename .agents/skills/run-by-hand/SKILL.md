---
name: run-by-hand
description: Run the real quartermaster binary by hand with a throwaway data directory, drive it in a pseudo-terminal (tmux) to see and check the screen, show the built-in sample screens, or use the hidden developer entry points (dev session, probe, leftovers, scripted seat). Use when you need to see a change working, reproduce a bug end to end, or check the UI.
---

# Running the app by hand

Always use a throwaway data directory; never the real `~/.quartermaster`:

```sh
export QUARTERMASTER_HOME=$(mktemp -d)
# Debug binaries only: isolate scratch too, without accessing /tmp/quartermaster.
export QM_DEV=1 QM_DEV_SCRATCH="$(mktemp -d)/scratch"
cargo run -p quartermaster -- --foreground  # one process; or build and run target/debug/quartermaster
```

On first run it creates the data directory (0700) and opens the Bridge; if `git`, `claude` or `gh`
is missing or logged out, a red dot shows on the menu (the setup check never opens by itself;
hide a tool with `PATH=/usr/bin:/bin QM_HOMEBREW_BINS=` to see it). `Ctrl`+`C` twice quits (it
asks first if agents work). A second `--foreground` on the same data directory says "already running"
and exits 1. The terminal must be at least 60×16.

Without `--foreground`, `target/debug/quartermaster` attaches to a background server for that
data directory (started on demand: a launchd one-shot job outside SSH, else a direct spawn;
`QM_NO_LAUNCHD=1` forces the direct spawn). Closing the window only detaches. Manage it with
`quartermaster server status|stop [--force]|restart`, and always `server stop` a throwaway one when
done (keep the data directory path short: macOS limits socket paths to 104 bytes). Logs say
`server listening`, `window attached`, `window detached`.

Claude accounts come from the user's own `claude` login (found at startup, never copied), so a
throwaway home still has Claude. Other accounts would need a login in Settings (`,`); never copy
`auth.json` from another data directory (refresh tokens rotate, logging the other copy out).
Live model calls cost money: pick the cheapest model (Haiku) for checks.

## Driving it in a pseudo-terminal

Use tmux, so you can send keys and read the screen:

```sh
tmux new-session -d -s qm -x 150 -y 45 "QUARTERMASTER_HOME=$QUARTERMASTER_HOME target/debug/quartermaster"
tmux send-keys -t qm 'hello' Enter        # type and send
tmux send-keys -t qm Tab                  # keys: Tab, Escape, Up, C-c, …
tmux capture-pane -p -t qm                # the screen as text
tmux capture-pane -p -e -t qm             # with escape sequences: check colours
tmux resize-window -t qm -x 119 -y 40     # fold the task pane
tmux kill-session -t qm
```

Use a private socket and no config (`tmux -f /dev/null -L <name>`): a `kill-server` on the default
socket ends every session on it, and the user's `~/.tmux.conf` plugins (tmux-continuum) can restore
their own sessions into yours. To paste an image (`Ctrl`+`V`) without touching your
clipboard, start the app with `QM_DEV_PASTEBOARD=<name>` and put the image on that named
pasteboard (`NSPasteboard pasteboardWithName:` from a JXA script via `osascript -l JavaScript`).

Check the sizes that change the layout: 150×45, 120×40 (unfolded), 119×40 and 100×30 (folded:
the task pane becomes an overlay), and 60×16 (the smallest). With `-e`, every colour must be a
16-colour SGR code (`30`–`37`, `90`–`97`, `40`–`47`, `100`–`107`), plus `38;2;`/`48;2;` only when
the app was started with `COLORTERM=truecolor` (or `24bit`); a `38;5;` is always a bug.
Mouse input is easiest to check in a real terminal window.

## Notifications

Notifications run a helper app from `$QUARTERMASTER_HOME/Quartermaster.app`; any home other than
`~/.quartermaster` gets the bundle ID `io.github.flotilla-labs.quartermaster.dev`, named "Quartermaster Dev", so
macOS asks for permission once for it and keeps its own Notifications settings (set Show previews to
Always, or banners say only "Notification"). It has only been checked with a short home under `~`
(`~/.qm-n`). Start with `QM_LOG=info,qm_notify=debug` to see `looking`, `banner posted`,
`banner held back`, `answered banners withdrawn` (skill `debug-from-logs`). A scripted seat
(`QM_DEV_SEAT_SCRIPT`) with an `ask_user` step makes a question on demand.

A banner is held back while a focused window's terminal app is the frontmost app. tmux forwards no
focus events by default and a window joins focused: send `tmux send-keys -l $'\e[O'` to mark it
unfocused. To check focus for real, open a WezTerm tab on the throwaway home with
`wezterm cli spawn -- /usr/bin/env QUARTERMASTER_HOME=… QM_DEV=1 … target/debug/quartermaster`, one
argument per variable: zsh doesn't split `env $VARS`, and a window without `QUARTERMASTER_HOME`
attaches to the user's real server. Drive it with `wezterm cli send-text --no-paste`, `get-text`
and `activate-pane`, and check which socket it reached (`lsof -U`) before typing. When done,
`lsregister -u` the bundle (`LaunchServices.framework/.../Support/lsregister`) and delete the home.

## Sample screens

`QM_DEV_SAMPLE` shows a fixed screen and ignores the core's snapshots (the picture holds still):
`bridge`, `chat`, `thinking`, `decision`, `diff` (the approval's split diff and changed-file tree),
`session`, `secondmate`, `settings` (the Settings
popup over the Bridge) or `menu-badge` (a badge on the menu's Settings entry), `compacting` (the Quartermaster compacting
its context, a failed compaction on C-0042) or `context-unknown` (no context readings), `empty` (an empty Bridge, for the chat header); all values are in
`crates/qm-tui/src/sample.rs`. Only the busy indicator still moves. Use them to
compare layout and styles without any account. tmux shows the `◆` fallback; the logo image needs
a kitty-graphics terminal (Ghostty, kitty, WezTerm) in a real window.

## Developer entry points (hidden)

Open only with `QM_DEV=1` and an explicit `QUARTERMASTER_HOME` that isn't `~/.quartermaster`
(`crates/quartermaster/src/dev/mod.rs`); otherwise the binary ignores them. They take the run lock, start
logging and open the store like a normal launch.

```sh
QM_DEV=1 quartermaster dev session --engine claude --role crewmate --model claude-haiku-5-5
    # one agent on a real engine, driven from stdin, every engine event printed
    # also: --engine provider|scripted, --script FILE, --account REF, --effort, --resume, --cwd
QM_DEV=1 quartermaster dev probe claude      # detect claude and run its zero-token startup check
QM_DEV=1 quartermaster dev leftovers         # the startup report of leftover process groups
QM_DEV=1 quartermaster dev login chatgpt --ref chatgpt:dev
QM_DEV=1 QM_DEV_SEAT_SCRIPT=seat.toml quartermaster   # the Quartermaster on the scripted engine
QM_DEV=1 QM_DEV_QUOTA_URL=http://127.0.0.1:<port>/q quartermaster  # debug builds: every
    # quota check (Settings ▸ Accounts) goes to your local stand-in instead of the provider;
    # only a loopback URL is accepted, since it receives the accounts' keys
QM_DEV=1 QM_DEV_RUNNING_VERSION=0.0.1 quartermaster  # look older, so the newest release shows
    # as an update (notice, Menu ▸ Update). Pressing Update really installs it over the running
    # binary's folder: run a copy from a throwaway <dir>/bin, with XDG_CONFIG_HOME=<throwaway>
    # for the installer's receipt; a copy in target/ is refused.
```

The scripted engine plays a TOML script (format in `crates/qm-engine/src/scripted/script.rs`):
text, tool calls, stalls, crashes, jobs and provider errors, with no model and no cost. It suits
reproducing turn-loop and UI behaviour.
A scripted seat still needs a model level (Settings ▸ Model levels), and its `start_crewmate`
titles must be conventional (`docs(notes): …`); add `expect_result_contains` to a tool call to see
why a refused call failed. Plan pages send to `127.0.0.1:<port>` from `<home>/plan-endpoint.json`
(`{"port":N}`); write it before starting so the endpoint binds inside your port block.

## After the run

Read the day's log in `$QUARTERMASTER_HOME/logs/` (skill `debug-from-logs`). Delete the
throwaway directory when done. With `QM_DEV_SCRATCH`, remove its unique parent directory too.
The override requires a debug binary, `QM_DEV=1`, and an explicit non-default home; unsafe overrides
fail instead of falling back to shared scratch. Without the override, a run makes its scratch root,
`/tmp/quartermaster/<uid>/<hash>/` (provider crewmates' scratchpads only; the hash is of
the data directory path). Remove only that run's `<hash>` folder, never the shared ancestors. Never paste credentials
from it into a report.
