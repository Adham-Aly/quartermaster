//! Entering and leaving full-screen mode.
//!
//! [`TerminalGuard::enter`] puts the terminal in raw mode on the alternate screen with mouse
//! capture (clicks, drags, the wheel, and motion tracking for hover, in SGR encoding) and
//! bracketed paste, and, where the terminal supports it, asks for disambiguated keys so `⇧⏎`
//! arrives as itself. It also asks the terminal for its colours (`theme::detect`), again on every
//! resume, so a theme changed meanwhile is picked up. Dropping the guard undoes all of it in reverse, and so does a panic: a hook
//! restores the terminal before the panic message prints, so you never land in a broken shell.
//! [`TerminalGuard::suspend`] does the same for `Ctrl`+`Z`, and enters again when the shell's
//! `fg` resumes the process.
//!
//! The escape sequences themselves come from [`write_enter`] and [`write_leave`], which write to
//! any `Write`.

use crate::backend::Backend16;
use crate::theme::{self, Palette};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the keyboard enhancement was pushed, so every way out (drop, panic, suspend) pops it.
static ENHANCED: AtomicBool = AtomicBool::new(false);

/// Writes the sequences that enter full-screen mode: the alternate screen, mouse capture with
/// motion tracking in SGR encoding, bracketed paste, and, when `enhanced`, the keyboard
/// enhancement that disambiguates escape codes. Raw mode is not a byte sequence and is not
/// included.
pub(crate) fn write_enter(w: &mut impl Write, enhanced: bool) -> io::Result<()> {
    queue!(
        w,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
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
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )?;
    w.flush()
}

/// The full-screen terminal, restored when dropped.
pub(crate) struct TerminalGuard {
    terminal: Terminal<Backend16<Stdout>>,
    /// The palette fitted to this terminal's colours.
    palette: Palette,
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
        let entered = switch_on().and_then(|palette| {
            Terminal::new(Backend16::new(io::stdout())).map(|terminal| (terminal, palette))
        });
        match entered {
            Ok((terminal, palette)) => Ok(Self { terminal, palette }),
            Err(err) => {
                restore();
                Err(err)
            }
        }
    }

    /// The palette fitted to this terminal's colours when it was last entered.
    pub(crate) const fn palette(&self) -> Palette {
        self.palette
    }

    /// Draws one frame inside a synchronized update (DEC mode 2026), so a terminal that supports
    /// it shows the frame all at once.
    pub(crate) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        draw_synchronized(&mut self.terminal, render)
    }

    /// Writes raw bytes to the terminal between frames (an OSC 52 copy).
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        let backend = self.terminal.backend_mut();
        backend.write_all(bytes)?;
        backend.flush()
    }

    /// `Ctrl`+`Z`: leaves full-screen mode, stops the process with `SIGTSTP` as the shell
    /// expects, and when the shell's `fg` continues it, enters again and clears the screen so the
    /// next frame redraws everything.
    pub(crate) fn suspend(&mut self) -> io::Result<()> {
        restore();
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP).map_err(io::Error::from)?;
        // The terminal's theme may have changed while the app was stopped.
        self.palette = switch_on()?;
        self.terminal.clear()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Draws one frame into `terminal` between `?2026h` and `?2026l` (see [`TerminalGuard::draw`]).
fn draw_synchronized<W: Write>(
    terminal: &mut Terminal<Backend16<W>>,
    render: impl FnOnce(&mut Frame<'_>),
) -> io::Result<()> {
    crossterm::execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal.draw(render).map(|_| ());
    let ended = crossterm::execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drawn.and(ended)
}

/// Raw mode, then the terminal's colours, then the enter sequences (with the keyboard
/// enhancement when supported). Returns the palette fitted to the colours.
fn switch_on() -> io::Result<Palette> {
    enable_raw_mode()?;
    // Before anything else reads the terminal's input, which would swallow the answers.
    let palette = theme::detect();
    // The query needs raw mode to read the terminal's answer.
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    ENHANCED.store(enhanced, Ordering::SeqCst);
    write_enter(&mut io::stdout(), enhanced)?;
    Ok(palette)
}

/// Leaves full-screen mode. Every step runs even if an earlier one fails, and errors are ignored:
/// this runs on the way out, where there is nobody left to tell.
fn restore() {
    let _ = disable_raw_mode();
    let enhanced = ENHANCED.swap(false, Ordering::SeqCst);
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
