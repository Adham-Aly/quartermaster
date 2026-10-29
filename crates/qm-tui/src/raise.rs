//! Bringing this window's own tab forward when a notification is clicked.
//!
//! The notifier brings the terminal app forward, but an app with several tabs shows whichever tab
//! was last in front. So the window you used last is told to raise itself ([`Control::Raise`]),
//! and the client, which runs inside that tab, asks its terminal to show it: tmux by the pane it
//! runs in, `WezTerm` and kitty by their pane and window IDs. Other terminals have no way to ask,
//! and keep the tab they show.
//!
//! [`Control::Raise`]: qm_types::wire::Control::Raise

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Asks this window's terminal to show its tab. Never waits, never fails: a terminal that
/// doesn't answer keeps the tab it shows.
pub(crate) fn this_tab() {
    let Some(argv) = command(&|name| std::env::var_os(name), &|path| path.is_file()) else {
        return;
    };
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        // Reaped on a thread of its own, so the window never waits on it.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// The command that shows this tab, from the client's environment (`var`), if its terminal has
/// one. Inside tmux only tmux knows where the window is: the outer terminal's IDs in the
/// environment are those of wherever the tmux server started.
fn command(
    var: &dyn Fn(&str) -> Option<OsString>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<Vec<OsString>> {
    let set = |name: &str| var(name).filter(|v| !v.is_empty());
    if set("TMUX").is_some() {
        let pane = set("TMUX_PANE")?;
        let mut argv: Vec<OsString> = vec!["tmux".into()];
        // The window, the pane in it, and the session the client shows; a failing step (no
        // client attached) stops only what comes after it.
        for (i, step) in ["select-window", "select-pane", "switch-client"]
            .iter()
            .enumerate()
        {
            if i > 0 {
                argv.push(";".into());
            }
            argv.extend([(*step).into(), "-t".into(), pane.clone()]);
        }
        return Some(argv);
    }
    if let Some(pane) = set("WEZTERM_PANE") {
        // The CLI sits beside the app's own executable; else the one on PATH.
        let beside = set("WEZTERM_EXECUTABLE_DIR")
            .map(|dir| Path::new(&dir).join("wezterm"))
            .filter(|path| is_file(path));
        let program = beside.map_or_else(|| "wezterm".into(), OsString::from);
        return Some(vec![
            program,
            "cli".into(),
            "activate-pane".into(),
            "--pane-id".into(),
            pane,
        ]);
    }
    if let (Some(window), Some(listen)) = (set("KITTY_WINDOW_ID"), set("KITTY_LISTEN_ON")) {
        let mut matching = OsString::from("id:");
        matching.push(window);
        return Some(vec![
            "kitty".into(),
            "@".into(),
            "--to".into(),
            listen,
            "focus-window".into(),
            "--match".into(),
            matching,
        ]);
    }
    None
}
