//! Pasting images: `Ctrl`+`V` in a chat input.
//!
//! `⌘V` is the terminal's own paste and stays text only: it arrives as a bracketed paste
//! (`App::on_paste`). `Ctrl`+`V` reaches the app as a key. It reads the images on the macOS
//! pasteboard off the UI thread (`osascript` running `clipboard_image.js`), inserts a placeholder
//! `[Image #N]` for each at the cursor and keeps the images with the draft, so each chat (the
//! Bridge, every worker's session) has its own. Sending passes the images whose placeholders are
//! still in your text with the message (`Command::Send { images }`); the core saves them under
//! `attachments/message-<id>/` and adds their paths to the message (`qm_core::attachments`).
//! Agents only ever get the paths.
//!
//! On the pasteboard, copied image files count first (the files, never their Finder icons), then
//! PNG, JPEG, GIF or WebP data as it is, then anything else `AppKit` can draw (TIFF, HEIC, PDF),
//! converted to PNG.
//!
//! `QM_DEV_PASTEBOARD` names a private pasteboard to read instead of the general one, so a
//! development run can be driven without touching your clipboard.

use crate::app::App;
use crate::hit::Pane;
use crate::router::Screen;
use qm_types::api::{ImageFormat, MAX_IMAGE_BYTES, PastedImage};
use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

/// The script `osascript` runs.
const SCRIPT: &str = include_str!("clipboard_image.js");

/// How long reading the pasteboard may take before it is given up.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The most images one message holds.
pub(crate) const MAX_IMAGES: usize = 20;

/// The development switch naming a private pasteboard.
const DEV_PASTEBOARD: &str = "QM_DEV_PASTEBOARD";

/// What the pasteboard held: each image's bytes, or why it couldn't be read.
type Read = Result<Vec<Vec<u8>>, String>;

/// A `Ctrl`+`V` whose pasteboard read hasn't come back yet.
#[derive(Debug)]
pub(crate) struct Pasting {
    rx: mpsc::Receiver<Read>,
    /// The chat it was pressed in: the answer goes into this draft only.
    screen: Screen,
}

/// Reads the images on the pasteboard (`pasteboard`, or the general one), in order.
fn read_pasteboard(pasteboard: Option<&str>) -> Read {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "quartermaster-paste-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("no temporary folder ({e})"))?;
    let read = run_script(&dir, pasteboard).and_then(|()| collect(&dir));
    let _ = fs::remove_dir_all(&dir);
    read
}

fn run_script(dir: &Path, pasteboard: Option<&str>) -> Result<(), String> {
    let mut command = Command::new("osascript");
    command
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(name) = pasteboard {
        command.arg(name);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("osascript didn't start ({e})"))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("osascript {status}")),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(String::from("it took too long"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The files `0`, `1`, … the script wrote, in order.
fn collect(dir: &Path) -> Read {
    let mut out = Vec::new();
    for n in 0.. {
        match fs::read(dir.join(n.to_string())) {
            Ok(bytes) => out.push(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

impl App {
    /// `Ctrl`+`V` in a chat input: reads the pasteboard's images, off the UI thread.
    pub(crate) fn paste_image(&mut self, now: Instant) {
        if self.view.guidance_for.is_some() || self.view.location_for.is_some() {
            self.flash("Images go with messages, not here", now);
            return;
        }
        if super::clipboard::over_ssh() {
            self.flash(
                "Pasting images needs this Mac's own clipboard, not SSH",
                now,
            );
            return;
        }
        if self.pasting.is_some() {
            return;
        }
        self.draft.prune_images();
        if self.draft.images.len() >= MAX_IMAGES {
            self.flash(format!("A message holds at most {MAX_IMAGES} images"), now);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let pasteboard = std::env::var(DEV_PASTEBOARD)
            .ok()
            .filter(|name| !name.is_empty());
        let spawned = std::thread::Builder::new()
            .name(String::from("paste-image"))
            .spawn(move || {
                let _ = tx.send(read_pasteboard(pasteboard.as_deref()));
            });
        if spawned.is_err() {
            self.flash("Couldn't read the clipboard", now);
            return;
        }
        self.pasting = Some(Pasting {
            rx,
            screen: self.screen,
        });
    }

    /// Takes the pasteboard's answer once it is in: placeholders at the cursor, images on the
    /// draft. Called every loop.
    pub(crate) fn paste_tick(&mut self, now: Instant) {
        let Some(pasting) = &self.pasting else { return };
        let read = match pasting.rx.try_recv() {
            Ok(read) => read,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(String::from("it stopped")),
        };
        let screen = pasting.screen;
        self.pasting = None;
        if self.screen != screen {
            self.flash("The image wasn't pasted: you left that chat", now);
            return;
        }
        let found = match read {
            Ok(found) if found.is_empty() => {
                self.flash("No image on the clipboard (⌘V pastes text)", now);
                return;
            }
            Ok(found) => found,
            Err(why) => {
                self.flash(format!("Couldn't read the clipboard: {why}"), now);
                return;
            }
        };
        self.draft.prune_images();
        let mut placeholders = Vec::new();
        let mut skipped = 0_usize;
        for bytes in found {
            let format = ImageFormat::sniff(&bytes);
            match format {
                Some(format)
                    if bytes.len() <= MAX_IMAGE_BYTES && self.draft.images.len() < MAX_IMAGES =>
                {
                    let number = self.draft.next_image_number();
                    let image = PastedImage::new(number, format, bytes);
                    placeholders.push(image.placeholder());
                    self.draft.images.push(image);
                }
                _ => skipped += 1,
            }
        }
        if !placeholders.is_empty() {
            self.focus_pane(Pane::Input);
            // Set apart from the word before it, and from what you type next.
            let from = self.selection().map_or(self.cursor(), |(from, _)| from);
            let joined = self.draft.text[..from]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace());
            let lead = if joined { " " } else { "" };
            self.insert(&format!("{lead}{} ", placeholders.join(" ")));
        }
        if skipped > 0 {
            let limit = MAX_IMAGE_BYTES / (1024 * 1024);
            self.flash(
                format!(
                    "{skipped} not pasted: over {limit} MB, past {MAX_IMAGES} images, or unreadable"
                ),
                now,
            );
        }
    }
}
