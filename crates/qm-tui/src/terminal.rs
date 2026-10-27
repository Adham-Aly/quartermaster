//! Entering and leaving full-screen mode.
//!
//! [`TerminalGuard::enter`] puts the terminal in raw mode on the alternate screen with mouse
//! capture (clicks, drags, the wheel, and motion tracking for hover, in SGR encoding), bracketed
//! paste and focus reports (coming back to the window re-runs the setup check), and, where the
//! terminal supports it, asks for disambiguated keys so `⇧⏎` arrives as itself. It also asks the
//! terminal for its colours (`theme::detect`), again on every resume, so a theme changed meanwhile
//! is picked up. Dropping the guard undoes all of it in reverse, and so does a panic: a hook
//! restores the terminal before the panic message prints, so you never land in a broken shell.
//! [`TerminalGuard::suspend`] does the same for `Ctrl`+`Z`, and enters again when the shell's
//! `fg` resumes the process.
//!
//! The escape sequences themselves come from [`write_enter`] and [`write_leave`], which write to
//! any `Write`.

use crate::backend::Backend16;
use crate::theme;
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use qm_types::wire::Colors;
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the keyboard enhancement was pushed, so every way out (drop, panic, suspend) pops it.
static ENHANCED: AtomicBool = AtomicBool::new(false);
/// Whether the terminal shows images, so every way out takes the logo away with it.
static GRAPHICS: AtomicBool = AtomicBool::new(false);

/// Writes the sequences that enter full-screen mode: the alternate screen, mouse capture with
/// motion tracking in SGR encoding, bracketed paste, focus reports, and, when `enhanced`, the keyboard
/// enhancement that disambiguates escape codes. Raw mode is not a byte sequence and is not
/// included.
pub(crate) fn write_enter(w: &mut impl Write, enhanced: bool) -> io::Result<()> {
    queue!(
        w,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if enhanced {
        queue!(
            w,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    w.flush()
}

/// Writes the sequences that undo [`write_enter`], in reverse order, and shows the cursor.
pub(crate) fn write_leave(w: &mut impl Write, enhanced: bool) -> io::Result<()> {
    if enhanced {
        queue!(w, PopKeyboardEnhancementFlags)?;
    }
    queue!(
        w,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )?;
    w.flush()
}

/// The full-screen terminal, restored when dropped. It only holds the terminal's modes and
/// writes bytes; what to draw is the host's business.
pub(crate) struct TerminalGuard {
    out: Stdout,
    /// The terminal's colours when it was last entered.
    colors: Colors,
}

impl std::fmt::Debug for TerminalGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalGuard").finish_non_exhaustive()
    }
}

impl TerminalGuard {
    /// Enters full-screen mode. On failure, whatever was already switched on is switched off.
    pub(crate) fn enter() -> io::Result<Self> {
        install_panic_hook();
        match switch_on() {
            Ok(colors) => Ok(Self {
                out: io::stdout(),
                colors,
            }),
            Err(err) => {
                restore();
                Err(err)
            }
        }
    }

    /// The terminal's colours when it was last entered.
    pub(crate) fn colors(&self) -> Colors {
        self.colors.clone()
    }

    /// Whether the keyboard enhancement was pushed, so `⇧⏎` arrives as itself.
    pub(crate) fn keyboard_enhanced() -> bool {
        ENHANCED.load(Ordering::SeqCst)
    }

    /// Writes bytes the host drew to the terminal, as they are.
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }

    /// `Ctrl`+`Z`: leaves full-screen mode, stops the process with `SIGTSTP` as the shell
    /// expects, and when the shell's `fg` continues it, enters again. The caller then asks the
    /// host for a full repaint. Returns the terminal's colours again: its theme may have changed
    /// while the app was stopped.
    pub(crate) fn suspend() -> io::Result<Colors> {
        restore();
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP).map_err(io::Error::from)?;
        switch_on()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Draws one frame into `terminal` between `?2026h` and `?2026l` (so a terminal that supports it shows the frame all at once).
pub(crate) fn draw_synchronized<W: Write>(
    terminal: &mut Terminal<Backend16<W>>,
    render: impl FnOnce(&mut Frame<'_>),
) -> io::Result<()> {
    crossterm::execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal.draw(render).map(|_| ());
    let ended = crossterm::execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drawn.and(ended)
}

/// Raw mode, then the terminal's colours, then the enter sequences (with the keyboard
/// enhancement when supported). Returns the colours the terminal reported.
fn switch_on() -> io::Result<Colors> {
    enable_raw_mode()?;
    // Before anything else reads the terminal's input, which would swallow the answers.
    let colors = theme::detect();
    GRAPHICS.store(colors.graphics, Ordering::SeqCst);
    // The query needs raw mode to read the terminal's answer.
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    ENHANCED.store(enhanced, Ordering::SeqCst);
    write_enter(&mut io::stdout(), enhanced)?;
    Ok(colors)
}

/// Leaves full-screen mode. Every step runs even if an earlier one fails, and errors are ignored:
/// this runs on the way out, where there is nobody left to tell.
fn restore() {
    let _ = disable_raw_mode();
    let enhanced = ENHANCED.swap(false, Ordering::SeqCst);
    if GRAPHICS.swap(false, Ordering::SeqCst) {
        // The host placed the logo; it isn't the shell's to keep.
        let _ = io::stdout().write_all(crate::logo::forget().as_bytes());
    }
    let _ = write_leave(&mut io::stdout(), enhanced);
}

/// Restores the terminal before the default panic message prints. Installed once per process.
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}
