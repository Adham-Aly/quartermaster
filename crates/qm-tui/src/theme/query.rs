//! Asking the terminal for its colours.
//!
//! At startup, in raw mode, the app asks for the default foreground (OSC 10), the default
//! background (OSC 11) and each of the 16 palette colours (OSC 4), then for the primary device
//! attributes (DA1). Every terminal answers DA1, and answers in order, so once its answer arrives
//! nothing else is coming: a terminal that ignores the colour queries costs no wait. Where the
//! terminal says nothing about its background, `COLORFGBG` (set by some terminals) still tells a
//! light background from a dark one.

use super::fit::{Reported, Rgb};
use nix::sys::termios::{self, SetArg, SpecialCharacterIndices};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

/// How long to wait for answers from a terminal that never answers DA1.
const TIMEOUT: Duration = Duration::from_millis(400);

/// What the terminal says about its colours, with `COLORFGBG` as the fallback for the background.
/// Call it in raw mode, before anything else reads the terminal's input.
pub(crate) fn ask() -> Reported {
    let mut reported = ask_terminal()
        .map(|bytes| parse(&bytes))
        .unwrap_or_default();
    if reported.bg.is_none() && reported.fg.is_none() {
        let light = std::env::var("COLORFGBG")
            .ok()
            .and_then(|v| colorfgbg_is_light(&v));
        reported.bg = light.map(|light| if light { Rgb::WHITE } else { Rgb::BLACK });
    }
    reported
}

/// Sends the queries to the controlling terminal and returns what came back before the DA1
/// answer (or the timeout).
fn ask_terminal() -> io::Result<Vec<u8>> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    // Reads give up after a tenth of a second of silence instead of waiting for a byte.
    let saved = termios::tcgetattr(&tty)?;
    let mut quick = saved.clone();
    quick.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    quick.control_chars[SpecialCharacterIndices::VTIME as usize] = 1;
    termios::tcsetattr(&tty, SetArg::TCSANOW, &quick)?;
    let answers = tty
        .write_all(queries().as_bytes())
        .and_then(|()| tty.flush())
        .map(|()| read_answers(&mut tty));
    let restored = termios::tcsetattr(&tty, SetArg::TCSANOW, &saved);
    let answers = answers?;
    restored?;
    Ok(answers)
}

/// The queries: foreground, background, the 16 palette colours, then DA1.
fn queries() -> String {
    let mut out = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for index in 0..16 {
        let _ = write!(out, "\x1b]4;{index};?\x1b\\");
    }
    out.push_str("\x1b[c");
    out
}

/// Reads until the DA1 answer arrives or the timeout passes.
fn read_answers(tty: &mut File) -> Vec<u8> {
    let deadline = Instant::now() + TIMEOUT;
    let mut out = Vec::new();
    let mut chunk = [0_u8; 1024];
    while Instant::now() < deadline && !has_da1(&out) {
        match tty.read(&mut chunk) {
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    out
}

/// Whether `bytes` hold a DA1 answer: `ESC [ ?` then digits and `;`, then `c`.
fn has_da1(bytes: &[u8]) -> bool {
    bytes.windows(3).enumerate().any(|(at, w)| {
        w == b"\x1b[?"
            && bytes[at + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                == Some(&b'c')
    })
}

/// The colours in the terminal's answers: OSC 10, 11 and 4 replies, ended by BEL or ST.
pub(crate) fn parse(bytes: &[u8]) -> Reported {
    let mut reported = Reported::default();
    let mut rest = bytes;
    while let Some(start) = find(rest, b"\x1b]") {
        rest = &rest[start + 2..];
        let end = rest
            .iter()
            .position(|b| *b == 0x07 || *b == 0x1b)
            .unwrap_or(rest.len());
        let body = String::from_utf8_lossy(&rest[..end]);
        rest = &rest[end..];
        let mut parts = body.split(';');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("10"), Some(spec), None) => reported.fg = parse_color(spec),
            (Some("11"), Some(spec), None) => reported.bg = parse_color(spec),
            (Some("4"), Some(index), Some(spec)) => {
                if let Some(slot) = index
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| reported.ansi.get_mut(i))
                {
                    *slot = parse_color(spec);
                }
            }
            _ => {}
        }
    }
    reported
}

/// The first position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// An X11 colour spec as terminals send it: `rgb:R/G/B` or `rgba:R/G/B/A` with one to four hex
/// digits a channel, or `#RRGGBB`.
fn parse_color(spec: &str) -> Option<Rgb> {
    let channels = if let Some(hex) = spec.strip_prefix("rgb:") {
        hex.split('/').collect::<Vec<_>>()
    } else if let Some(hex) = spec.strip_prefix("rgba:") {
        hex.split('/').take(3).collect()
    } else {
        let hex = spec.strip_prefix('#')?;
        if hex.len() != 6 {
            return None;
        }
        vec![&hex[0..2], &hex[2..4], &hex[4..6]]
    };
    let [r, g, b] = channels.as_slice() else {
        return None;
    };
    Some(Rgb::new(channel(r)?, channel(g)?, channel(b)?))
}

/// One channel of one to four hex digits, scaled to 0–255.
fn channel(hex: &str) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1_u32 << (4 * hex.len())) - 1;
    u8::try_from((value * 255 + max / 2) / max).ok()
}

/// `COLORFGBG` is `fg;bg` (or `fg;default;bg`) in palette indices; the background is light when
/// it is white (7) or a bright colour other than bright black (8).
fn colorfgbg_is_light(value: &str) -> Option<bool> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    match bg {
        7 | 9..=15 => Some(true),
        0..=6 | 8 => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_answers_ended_by_st_or_bel() {
        let answers = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b]10;rgb:00/00/00\x07\
            \x1b]4;8;rgb:a0a0/a1a1/a7a7\x1b\\\x1b]4;3;rgb:c/b/1\x1b\\\x1b[?1;2;4c";
        let reported = parse(answers);
        assert_eq!(reported.bg, Some(Rgb::new(255, 255, 255)));
        assert_eq!(reported.fg, Some(Rgb::new(0, 0, 0)));
        assert_eq!(reported.ansi[8], Some(Rgb::new(0xa0, 0xa1, 0xa7)));
        assert_eq!(reported.ansi[3], Some(Rgb::new(0xcc, 0xbb, 0x11)));
        assert_eq!(reported.ansi[0], None);
        assert!(has_da1(answers));
    }

    #[test]
    fn ignores_what_it_cannot_read() {
        let reported = parse(b"\x1b]11;?\x1b\\\x1b]4;99;rgb:1/2/3\x07\x1b]52;c;aGk=\x07junk");
        assert_eq!(reported, Reported::default());
        assert_eq!(parse_color("#336699"), Some(Rgb::new(0x33, 0x66, 0x99)));
        assert_eq!(
            parse_color("rgba:ffff/0000/8080/ffff"),
            Some(Rgb::new(255, 0, 128))
        );
        assert_eq!(parse_color("rgb:12345/0/0"), None);
        assert_eq!(parse_color("red"), None);
    }

    #[test]
    fn da1_needs_its_final_c() {
        assert!(has_da1(b"\x1b[?62;22c"));
        assert!(!has_da1(b"\x1b[?62;22"));
        assert!(!has_da1(b"\x1b[?62;22$y"));
    }

    #[test]
    fn colorfgbg_tells_light_from_dark() {
        assert_eq!(colorfgbg_is_light("0;15"), Some(true));
        assert_eq!(colorfgbg_is_light("0;default;7"), Some(true));
        assert_eq!(colorfgbg_is_light("15;0"), Some(false));
        assert_eq!(colorfgbg_is_light("7;8"), Some(false));
        assert_eq!(colorfgbg_is_light("default;default"), None);
    }
}
