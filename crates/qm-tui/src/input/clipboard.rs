//! Copying to the macOS pasteboard.
//!
//! On this Mac the text goes to `pbcopy`, found on `PATH`. Over SSH, where `pbcopy` would fill the
//! remote Mac's pasteboard, the app writes OSC 52 instead, which the terminal you sit at turns
//! into a copy; it does the same if `pbcopy` fails.

use crate::app::App;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 §4).
pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 sequence that copies `text` to the clipboard: `ESC ] 52 ; c ; <base64> BEL`.
pub(crate) fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Whether a session with these `SSH_TTY` and `SSH_CONNECTION` values is over SSH.
pub(crate) fn is_ssh(ssh_tty: Option<&str>, ssh_connection: Option<&str>) -> bool {
    ssh_tty.is_some_and(|v| !v.is_empty()) || ssh_connection.is_some_and(|v| !v.is_empty())
}

/// Whether `term`'s window is over SSH.
pub(crate) fn over_ssh(term: &crate::term::Term) -> bool {
    term.ssh
}

/// Pipes `text` to `pbcopy` on `PATH`, with nothing reaching the screen.
pub(crate) fn pbcopy(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy")
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("pbcopy has no stdin"))
        .and_then(|mut stdin| stdin.write_all(text.as_bytes()));
    let status = child.wait()?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("pbcopy exited with {status}")))
    }
}

/// "Copied", or "Copied N lines" for more than one.
pub(crate) fn copied_notice(text: &str) -> String {
    match text.lines().count() {
        0 | 1 => String::from("Copied"),
        n => format!("Copied {n} lines"),
    }
}

impl App {
    /// Copies `text`: `pbcopy` on this Mac, OSC 52 over SSH or when `pbcopy` fails, then flashes
    /// the notice.
    pub(crate) fn copy(&mut self, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        if over_ssh(&self.term) || pbcopy(text).is_err() {
            self.term.osc52 = Some(osc52(text));
        }
        self.flash(copied_notice(text), now);
    }
}
