//! The terminal UI.
//!
//! The Bridge layout, settings and login screens, decision and approval cards, worker sessions, and
//! the setup check, on `ratatui` with `crossterm` on the alternate screen, mouse first-class and
//! keyboard complete, drawn in the terminal's own 16-colour palette.
//!
//! This crate depends on `qm-types` only. It talks to the core exclusively through `qm_types::api`:
//! it sends `Command`s and renders the `Update`s it receives.
//!
//! # Host and client
//!
//! The UI is split in two, joined by a byte stream carrying `qm_types::wire` frames. The
//! [`host`] owns the one `App` and draws it once per attached window (each window's state is a
//! `Term`), at that window's size, into bytes. [`attach`] is the thin client: it owns the real
//! terminal, forwards keys and the mouse as `wire::InputEvent`s, and prints the host's bytes.
//! [`run`] joins one host and one client over a `tokio::io::duplex`; a server can later accept
//! sockets instead.

//!
//! # Layout of the crate
//!
//! - [`run`] (here) is the one entry point; `app` holds the state, `host` drives it for each window
//!   and `attach` is the client; `convert` turns crossterm events into wire input and back.
//! - `persist` saves the shared UI state to `ui.json` (debounced, atomic) and loads it at start.
//! - `term` holds one window's state (size, hit map, hover, selection, quit progress).
//! - `terminal` enters and leaves full-screen mode (raw mode, alternate screen, mouse capture
//!   with motion, bracketed paste) and restores the terminal on drop and on panic. Every frame is
//!   wrapped in a synchronized update (DEC mode 2026), so it appears all at once.
//! - `router` is the screen router; `chrome` draws the top bar (its menu button), the status
//!   bar and quit dialog around every screen; `bridge` draws the Bridge; `popups` opens
//!   Settings (`settings`), Help (`placeholder`) and Update (`update`) as popup windows over the
//!   screen, from the top bar's menu or their keys.
//! - `widgets` holds the Bridge's panes and their parts: projects, crew, conversation, cards,
//!   streaming, the busy indicator, the input box, the task pane and buttons; and the popup window
//!   and dropdown list any screen can use.
//! - `hit` is the per-frame hit map that drives clicks, drags, the wheel and hover alike.
//! - `input` turns keys and the mouse into changes: focus and the key map, clicks, double-clicks,
//!   the wheel, hover, divider drags and column widths, context menus,
//!   the app's own selection and copy (`pbcopy`, or OSC 52 over SSH), and editing your message.
//! - `theme` holds the 16-colour styles; `text` measures and wraps in terminal cells.
//!
//! # Starting and quitting
//!
//! On start the UI sends `Command::Refresh` and draws `Snapshot::default()` (the empty Bridge)
//! until the core's snapshot arrives. `Ctrl`+`C` once shows "Press Ctrl+C again to quit" for two
//! seconds; a second press sends `Command::RequestQuit`, and the UI returns on `Update::Closing`,
//! or shows the quit dialog on `Update::ConfirmQuit` (`⏎` sends `Command::Quit`). A third press
//! while waiting for the core leaves anyway, so a stuck core can never trap you. The UI also
//! returns when the core drops its end of the link.
//!
//! Under a background server ([`host::serve`]) a second `Ctrl`+`C` only detaches that window; the
//! server is stopped from the menu's Stop server (Restart server and Detach are there too) or the
//! command line (`quartermaster server stop`, through [`host::Handle`]). A client newer than the
//! server it reaches (same protocol) draws a one-row notice of its own with a Restart button below
//! the host's window ([`Notice`]); an installed update says the same in the Update popup.

mod app;
mod attach;
mod backend;
mod bridge;
mod chrome;
mod convert;
mod diff;
mod hit;
pub mod host;
mod input;
mod persist;
mod placeholder;
mod popups;
mod router;
#[doc(hidden)]
pub mod sample;
mod session;
mod settings;
mod setup;
mod term;
mod terminal;
mod text;
mod theme;
mod update;
mod view;
mod widgets;

use qm_types::api::CoreClient;

pub use attach::{Left, Notice, attach};

/// Runs the terminal UI on the current thread until you quit or the core closes the link, then
/// restores the terminal. The binary calls it with its end of `qm_types::api::link()` while the
/// core runs on the Tokio runtime.
///
/// The host and this terminal's client run here, joined by an in-process byte stream, so it
/// looks and behaves as a UI that draws straight to the terminal.
///
/// The signature is the seam between `crates/quartermaster/src/main.rs` and this crate, so it stays
/// as it is.
pub fn run(client: CoreClient) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let (near, far) = tokio::io::duplex(1 << 20);
        let (windows_tx, windows) = tokio::sync::mpsc::unbounded_channel();
        let _ = windows_tx.send(Box::new(near) as Box<dyn host::Conn>);
        // No other window can arrive, so the host ends when this one leaves.
        drop(windows_tx);
        let (hosted, attached) = tokio::join!(host::run(client, windows), attach(far, None));
        hosted.and(attached.map(drop))
    })
}
