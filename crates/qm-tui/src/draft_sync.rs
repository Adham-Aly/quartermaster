//! Your unsent drafts, between the UI and the core. The core holds them for as long as the server
//! runs (`qm_core::drafts`), never on disk. This module sends each draft as it changes, restores
//! them from the first snapshot, and keeps the bytes of their pasted images in the data
//! directory's drafts folder (`DataDir::draft_images`), where a draft names each image by its token.
//!
//! - **Restore.** The first snapshot brings every draft back. An image comes back only when its file
//!   is there, is the size and format the draft says, and its placeholder is still in the text; any
//!   other image is dropped. A restored draft never replaces one you have typed since.
//! - **Sending.** [`DraftSync::tick`] sends a draft at once when it changes; an emptied draft is sent
//!   empty, which the core forgets. A draft's image tokens (a hash of the bytes and their length) are
//!   computed from content, even when image numbers and lengths are unchanged.
//! - **Files.** An image is written when missing or different, to `<token>.<ext>`. Files that no
//!   sent draft names are then removed. Nothing is written or removed before the restore, so the files
//!   the restore reads are never taken for unnamed.
//! - **Never logged.** A draft is your unsent words; errors are dropped, not reported.

use crate::app::App;
use crate::input::draft::Draft;
use crate::persist::write_atomic;
use crate::session::SessionUi;
use qm_types::api::{
    AgentRef, Command, DraftImage, DraftTarget, ImageFormat, MAX_IMAGE_BYTES, PastedImage,
    SavedDraft, image_placeholder,
};
use qm_types::data_dir::{DataDir, private_folder};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::thread::JoinHandle;

/// One job for the image writer: the new image files, and every file a sent draft still names.
struct Job {
    images: Vec<(String, Vec<u8>)>,
    keep: HashSet<String>,
}

/// Sends the drafts to the core and keeps their image files.
#[derive(Debug)]
pub(crate) struct DraftSync {
    dir: DataDir,
    results: mpsc::Receiver<bool>,
    jobs: Option<mpsc::Sender<Job>>,
    writer: Option<JoinHandle<()>>,
    /// Whether the first snapshot has brought the drafts back. Nothing is sent before that.
    restored: bool,
    /// What the core was last sent for each prompt.
    sent: HashMap<DraftTarget, SavedDraft>,
    /// Set by the restore: the next tick writes a cleanup even if no draft changed, so a file
    /// that no draft names any more is removed.
    cleanup_due: bool,
}

impl DraftSync {
    /// Starts the writer for `dir`'s image folder.
    pub(crate) fn open(dir: &DataDir) -> Self {
        let folder = dir.clone();
        let (finished, results) = mpsc::channel();
        let (jobs, queue) = mpsc::channel::<Job>();
        let writer = {
            let folder = folder.clone();
            std::thread::Builder::new()
                .name("draft-images".to_owned())
                .spawn(move || {
                    for job in queue {
                        let _ = finished.send(write_job(&folder, &job).is_ok());
                    }
                })
                .ok()
        };
        Self {
            dir: folder,
            results,
            jobs: writer.is_some().then_some(jobs),
            writer,
            restored: false,
            sent: HashMap::new(),
            cleanup_due: false,
        }
    }

    /// Puts the drafts from the first snapshot into `app`. Later snapshots are ignored: the UI is
    /// the one that changes a draft from then on.
    pub(crate) fn restore(&mut self, app: &mut App, saved: &[SavedDraft]) {
        if std::mem::replace(&mut self.restored, true) {
            return;
        }
        for draft in saved {
            let restored = self.read(draft);
            place(app, draft.target, restored);
        }
        // Everything is sent again on the next tick, so the core holds exactly what is shown.
        self.sent.clear();
        self.cleanup_due = true;
    }

    /// Sends each draft that changed since the last tick, forgets the ones that are gone, and
    /// writes the image files that are new.
    pub(crate) fn tick(&mut self, app: &mut App) {
        if !self.restored || app.view.sample {
            return;
        }
        // Failed writes/cleanup are retried even when the draft itself has not changed.
        for success in self.results.try_iter() {
            self.cleanup_due |= !success;
        }
        let mut changes = Vec::new();
        let mut files = Vec::new();
        let mut live = HashSet::new();
        for (target, draft) in targets(app) {
            if draft.is_empty() {
                continue;
            }
            live.insert(target);
            let (saved, new_files) = Self::build(target, draft);
            files.extend(new_files);
            if self.sent.get(&target) != Some(&saved) {
                self.sent.insert(target, saved.clone());
                changes.push(saved);
            }
        }
        let gone: Vec<DraftTarget> = self
            .sent
            .keys()
            .filter(|target| !live.contains(target))
            .copied()
            .collect();
        for target in gone {
            self.sent.remove(&target);
            changes.push(SavedDraft::new(target, ""));
        }
        if changes.is_empty() && !std::mem::take(&mut self.cleanup_due) {
            return;
        }
        self.cleanup_due = false;
        self.submit(files);
        for draft in changes {
            app.command(Command::SaveDraft { draft });
        }
    }

    /// Build tokens from content, not image numbers or byte lengths: a number can be reused
    /// between ticks for entirely different bytes. Include all live files so retries and cleanup
    /// never depend on a speculative "on disk" cache.
    fn build(target: DraftTarget, draft: &Draft) -> (SavedDraft, Vec<(String, Vec<u8>)>) {
        let mut files = Vec::new();
        let images: Vec<DraftImage> = draft
            .images
            .iter()
            .filter(|image| draft.text.contains(&image.placeholder()))
            .map(|image| {
                let bytes = image.bytes.as_slice();
                let token = token_of(bytes);
                files.push((file_name(&token, image.format), bytes.to_vec()));
                DraftImage::new(image.number, image.format, bytes.len(), token)
            })
            .collect();
        let mut saved = SavedDraft::new(target, draft.text.clone());
        saved.cursor = draft.cursor;
        saved.anchor = draft.anchor;
        saved.images = images;
        (saved, files)
    }

    /// The saved draft, with only images whose files are safe and match their metadata.
    fn read(&self, saved: &SavedDraft) -> Draft {
        let mut draft = Draft::new(saved.text.clone());
        draft.cursor = saved.cursor;
        draft.anchor = saved.anchor;
        if safe_folder(&self.dir).is_err() {
            return draft;
        }
        for image in &saved.images {
            if !token_ok(&image.token) || !draft.text.contains(&image_placeholder(image.number)) {
                continue;
            }
            let name = file_name(&image.token, image.format);
            let path = self.dir.draft_images().join(&name);
            if safe_folder(&self.dir).is_err() {
                break;
            }
            let small_enough = fs::symlink_metadata(&path)
                .is_ok_and(|m| m.is_file() && m.len() <= MAX_IMAGE_BYTES as u64);
            let Some(bytes) = small_enough.then(|| fs::read(&path).ok()).flatten() else {
                continue;
            };
            if bytes.len() != image.len || ImageFormat::sniff(&bytes) != Some(image.format) {
                continue;
            }
            draft
                .images
                .push(PastedImage::new(image.number, image.format, bytes));
        }
        draft
    }

    /// Hands the new image files to the writer, with the names every sent draft still needs.
    fn submit(&mut self, files: Vec<(String, Vec<u8>)>) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let keep: HashSet<String> = self
            .sent
            .values()
            .flat_map(|draft| &draft.images)
            .map(|image| file_name(&image.token, image.format))
            .collect();
        let _ = jobs.send(Job {
            images: files,
            keep,
        });
    }
}

impl Drop for DraftSync {
    /// Lets the writer finish what is queued.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Every prompt's draft, as the UI holds it: the Quartermaster's (parked while a session shows),
/// the shown session's, and the other sessions' drafts.
fn targets(app: &App) -> Vec<(DraftTarget, &Draft)> {
    let ui = app.view.session.as_ref();
    let shown = app.shown_session();
    let mut out = Vec::new();
    match shown {
        Some(agent) => {
            if let Some(ui) = ui {
                out.push((DraftTarget::Bridge, &ui.bridge_draft));
            }
            out.push((session(agent), &app.draft));
        }
        None => out.push((DraftTarget::Bridge, &app.draft)),
    }
    if let Some(ui) = ui {
        for (agent, draft) in &ui.drafts {
            if Some(*agent) != shown {
                out.push((session(*agent), draft));
            }
        }
    }
    out
}

fn session(agent: AgentRef) -> DraftTarget {
    DraftTarget::Session { agent }
}

/// Puts a restored draft where its prompt is, unless something is typed there already.
fn place(app: &mut App, target: DraftTarget, draft: Draft) {
    let shown = app.shown_session();
    match target {
        DraftTarget::Bridge if shown.is_some() => {
            let ui = app.view.session.get_or_insert_with(SessionUi::default);
            if ui.bridge_draft.is_empty() {
                ui.bridge_draft = draft;
            }
        }
        DraftTarget::Bridge => {
            if app.draft.is_empty() {
                app.draft = draft;
            }
        }
        DraftTarget::Session { agent } if shown == Some(agent) => {
            if app.draft.is_empty() {
                app.draft = draft;
            }
        }
        DraftTarget::Session { agent } => {
            let ui = app.view.session.get_or_insert_with(SessionUi::default);
            let slot = ui.drafts.entry(agent).or_default();
            if slot.is_empty() {
                *slot = draft;
            }
        }
        // A target from a newer core has no prompt here to put it in.
        _ => {}
    }
}

/// A name for an image's bytes: an FNV-1a hash and the length, so the same image is one file and
/// a different one never reuses it.
fn token_of(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}-{:x}", bytes.len())
}

fn file_name(token: &str, format: ImageFormat) -> String {
    format!("{token}.{}", format.extension())
}

/// Whether `token` is one `token_of` makes: nothing that could name another folder.
fn token_ok(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 40
        && token.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Writes the job's new images, then removes the files in the folder that no sent draft names.
fn write_job(dir: &DataDir, job: &Job) -> io::Result<()> {
    safe_folder(dir)?;
    let folder = dir.draft_images();
    for (name, bytes) in &job.images {
        safe_folder(dir)?;
        let path = folder.join(name);
        safe_file(&path)?;
        safe_file(&folder.join(format!("{name}.tmp")))?;
        if fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
            write_atomic(&path, bytes)?;
        }
    }
    remove_unnamed(dir, &job.keep)
}

/// Validate the hierarchy on every operation, including cleanup with no new images. The shared
/// ancestors are owned by `DataDir`'s validator; the drafts leaf must also be a private real folder.
fn safe_folder(dir: &DataDir) -> io::Result<()> {
    private_folder(dir.root())?;
    private_folder(&dir.draft_images())
}

/// Refuse symlinks (including dangling links), directories and special files at image/temp leaves.
fn safe_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(_) => Err(io::Error::other("draft image is not a plain file")),
    }
}

/// Removes only ordinary unnamed files, never following a drafts-directory or image symlink.
fn remove_unnamed(dir: &DataDir, keep: &HashSet<String>) -> io::Result<()> {
    safe_folder(dir)?;
    for entry in fs::read_dir(dir.draft_images())? {
        let entry = entry?;
        let named = entry.file_name().to_str().is_some_and(|n| keep.contains(n));
        if !named && entry.file_type()?.is_file() {
            safe_folder(dir)?;
            safe_file(&entry.path())?;
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
