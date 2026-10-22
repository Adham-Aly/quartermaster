//! Your unsent drafts, between the UI and the core. The core holds them for as long as the server
//! runs (`qm_core::drafts`), never on disk. This module sends each draft as it changes, restores
//! them from the first snapshot, and keeps the bytes of their pasted images in the scratch drafts
//! folder (`DataDir::draft_images`), where a draft names each image by its token.
//!
//! - **Restore.** The first snapshot brings every draft back. An image comes back only when its file
//!   is there, is the size and format the draft says, and its placeholder is still in the text; any
//!   other image is dropped. A restored draft never replaces one you have typed since.
//! - **Sending.** [`DraftSync::tick`] sends a draft at once when it changes; an emptied draft is sent
//!   empty, which the core forgets. A draft's image tokens (a hash of the bytes and their length) are
//!   computed only when its set of images changes.
//! - **Files.** A new image is written once, to `<token>.<ext>`. After each write, files that no
//!   sent draft names are removed. Nothing is written or removed before the restore, so the files
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
use qm_types::data_dir::DataDir;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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
    dir: PathBuf,
    jobs: Option<mpsc::Sender<Job>>,
    writer: Option<JoinHandle<()>>,
    /// Whether the first snapshot has brought the drafts back. Nothing is sent before that.
    restored: bool,
    /// What the core was last sent for each prompt.
    sent: HashMap<DraftTarget, SavedDraft>,
    /// The image files in the folder: written this run, or found there at the restore.
    on_disk: HashSet<String>,
    /// Set by the restore: the next tick writes a cleanup even if no draft changed, so a file
    /// that no draft names any more is removed.
    cleanup_due: bool,
}

impl DraftSync {
    /// Starts the writer for `dir`'s image folder.
    pub(crate) fn open(dir: &DataDir) -> Self {
        let folder = dir.draft_images();
        let (jobs, queue) = mpsc::channel::<Job>();
        let writer = {
            let folder = folder.clone();
            std::thread::Builder::new()
                .name("draft-images".to_owned())
                .spawn(move || {
                    for job in queue {
                        write_job(&folder, &job);
                    }
                })
                .ok()
        };
        Self {
            dir: folder,
            jobs: writer.is_some().then_some(jobs),
            writer,
            restored: false,
            sent: HashMap::new(),
            on_disk: HashSet::new(),
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
            let (restored, names) = self.read(draft);
            self.on_disk.extend(names);
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
        let mut changes = Vec::new();
        let mut files = Vec::new();
        let mut live = HashSet::new();
        for (target, draft) in targets(app) {
            if draft.is_empty() {
                continue;
            }
            live.insert(target);
            let (saved, new_files) = self.build(target, draft);
            if self.sent.get(&target) != Some(&saved) {
                self.sent.insert(target, saved.clone());
                changes.push(saved);
                files.extend(new_files);
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

    /// The draft as the core should hold it, and the image files it needs that aren't on disk. An
    /// image keeps its token while the set of images is unchanged: the bytes don't change.
    fn build(&self, target: DraftTarget, draft: &Draft) -> (SavedDraft, Vec<(String, Vec<u8>)>) {
        let shown: Vec<&PastedImage> = draft
            .images
            .iter()
            .filter(|image| draft.text.contains(&image.placeholder()))
            .collect();
        let previous = self.sent.get(&target);
        let same_images = previous.is_some_and(|p| {
            p.images.len() == shown.len()
                && p.images.iter().zip(&shown).all(|(sent, image)| {
                    sent.number == image.number
                        && sent.format == image.format
                        && sent.len == image.bytes.as_slice().len()
                })
        });
        let mut files = Vec::new();
        let images: Vec<DraftImage> = shown
            .iter()
            .enumerate()
            .map(|(i, image)| {
                let bytes = image.bytes.as_slice();
                let token = match previous {
                    Some(p) if same_images => p.images[i].token.clone(),
                    _ => token_of(bytes),
                };
                let name = file_name(&token, image.format);
                if !same_images && !self.on_disk.contains(&name) {
                    files.push((name, bytes.to_vec()));
                }
                DraftImage::new(image.number, image.format, bytes.len(), token)
            })
            .collect();
        let mut saved = SavedDraft::new(target, draft.text.clone());
        saved.cursor = draft.cursor;
        saved.anchor = draft.anchor;
        saved.images = images;
        (saved, files)
    }

    /// The draft a saved one holds, with each image whose file is there and is what it says, and
    /// the names of the files it uses.
    fn read(&self, saved: &SavedDraft) -> (Draft, Vec<String>) {
        let mut draft = Draft::new(saved.text.clone());
        draft.cursor = saved.cursor;
        draft.anchor = saved.anchor;
        let mut names = Vec::new();
        for image in &saved.images {
            if !token_ok(&image.token) || !draft.text.contains(&image_placeholder(image.number)) {
                continue;
            }
            let name = file_name(&image.token, image.format);
            let path = self.dir.join(&name);
            let small_enough = fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_IMAGE_BYTES as u64);
            let Some(bytes) = small_enough.then(|| fs::read(&path).ok()).flatten() else {
                continue;
            };
            if bytes.len() != image.len || ImageFormat::sniff(&bytes) != Some(image.format) {
                continue;
            }
            draft
                .images
                .push(PastedImage::new(image.number, image.format, bytes));
            names.push(name);
        }
        (draft, names)
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
        let images: Vec<(String, Vec<u8>)> = files
            .into_iter()
            .filter(|(name, _)| self.on_disk.insert(name.clone()))
            .collect();
        let _ = jobs.send(Job { images, keep });
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
fn write_job(folder: &Path, job: &Job) {
    if !job.images.is_empty() && make_dir(folder).is_ok() {
        for (name, bytes) in &job.images {
            let path = folder.join(name);
            if !path.exists() {
                let _ = write_atomic(&path, bytes);
            }
        }
    }
    remove_unnamed(folder, &job.keep);
}

fn make_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Removes the files in `dir` that `keep` doesn't name.
fn remove_unnamed(dir: &Path, keep: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let named = entry.file_name().to_str().is_some_and(|n| keep.contains(n));
        if !named && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests;
