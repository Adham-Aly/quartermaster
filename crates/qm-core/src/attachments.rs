//! Images you pasted into a chat with `Ctrl`+`V`, saved where agents can open them.
//!
//! # Where they go
//!
//! Your message brings its images ([`PastedImage`]) from the UI, each with its placeholder
//! (`[Image #1]`) in your text. Before the message is stored, [`Attachments::stage`] writes them to
//! a staging folder (`attachments/.staging-…/`). In the transaction that stores the message,
//! [`Staged::attach`] renames that folder to `attachments/message-<id>/` and adds one line per image
//! to the message's text:
//!
//! ```text
//! [Image #1]: ~/.quartermaster/attachments/message-42/image-1.png
//! ```
//!
//! The path is then part of what you said: the chat shows it, the agent you wrote to reads it, a
//! brief's copy of your words carries it, and orchestrators pass it on in briefs and messages.
//! Agents never get image bytes from the app; a crewmate opens the file with its file reader. This
//! is the same for the Quartermaster, a secondmate or a crewmate.
//!
//! # Cleanup policy
//!
//! [`Attachments::sweep`] runs at startup and then every [`SWEEP_EVERY`]. It removes:
//!
//! 1. **Unused.** A `message-<id>` folder that no open work mentions (no open task's goal, words,
//!    instructions, constraints, later steps or crewmate messages, and no open secondmate request)
//!    and no live chat holds (the message went to an agent that hasn't retired), once it is
//!    [`LEFTOVER_AFTER`] old. A staging folder, or a folder whose message doesn't name it, goes
//!    the same way, unless open work names it: what a crash between writing and storing leaves.
//! 2. **Over the cap.** While what is left holds more than [`SIZE_CAP`], the oldest folders held
//!    only by a chat go first, except those from the last [`ALWAYS_KEEP`]. Open work's are never
//!    removed for size.
//!
//! Anything else in the folder is left alone. That includes `drafts/` (`DataDir::draft_images`):
//! the images in your unsent drafts, which the UI host keeps for `ui.json` and removes itself once
//! no saved draft references them. So a draft's image survives the sweep, and a restart.

use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::messages;
use qm_types::api::{MAX_IMAGE_BYTES, PastedImage};
use qm_types::clock::Clock;
use qm_types::data_dir::{ensure_scratch_folder, migrate_directory};
use qm_types::ids::MessageId;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The most the attachments folder should hold, in bytes.
pub const SIZE_CAP: u64 = 1024 * 1024 * 1024;
/// Images this recent are never removed for the size cap.
pub const ALWAYS_KEEP: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);
/// When a staging or orphaned folder counts as left over.
pub const LEFTOVER_AFTER: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);
/// How often the sweep runs after the one at startup.
pub const SWEEP_EVERY: Duration = Duration::from_hours(1);

const STAGING_PREFIX: &str = ".staging-";
const FOLDER_PREFIX: &str = "message-";

/// Numbers staging folders apart within a run.
static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// A message's folder name: `message-<id>`.
pub fn folder_name(message: MessageId) -> String {
    format!("{FOLDER_PREFIX}{message}")
}

/// Where pasted images are saved, and their cleanup.
#[derive(Clone)]
pub struct Attachments {
    root: PathBuf,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Attachments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachments")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Attachments {
    /// `root` is the data directory's `attachments` folder, created when first needed.
    pub fn new(root: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { root, clock }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes the images whose placeholder is still in `text` to a fresh staging folder. An image
    /// that can't be saved is noted, never fatal: your message still goes, saying so.
    pub async fn stage(&self, text: &str, images: &[PastedImage]) -> Staged {
        let mut numbers = BTreeSet::new();
        let used: Vec<PastedImage> = images
            .iter()
            .filter(|image| text.contains(&image.placeholder()) && numbers.insert(image.number))
            .cloned()
            .collect();
        if used.is_empty() {
            return Staged::none(self.root.clone());
        }
        let name = format!(
            "{STAGING_PREFIX}{}-{}",
            self.clock.now().as_millisecond(),
            NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
        );
        let root = self.root.clone();
        let numbers: Vec<u32> = used.iter().map(|image| image.number).collect();
        match tokio::task::spawn_blocking(move || write_staged(root, &name, used)).await {
            Ok(staged) => staged,
            Err(e) => {
                tracing::error!(error = %e, "saving your pasted images stopped");
                Staged {
                    root: self.root.clone(),
                    dir: None,
                    images: numbers
                        .into_iter()
                        .map(|n| (n, Err(String::from("the app stopped while saving it"))))
                        .collect(),
                }
            }
        }
    }

    /// Keeps the message numbers the folders here already name out of new messages. Messages live
    /// in memory, so after a restart a folder can outlive its message while open work still
    /// names it; a new message with that number would otherwise take the folder. Runs on the
    /// database thread before anything can store a message (`qm-store` `ids::reserve`).
    pub async fn reserve_numbers(&self, db: &Db, legacy: &Path) {
        if ensure_scratch_folder(&self.root).is_err() {
            return;
        }
        let (root, old) = (self.root.clone(), legacy.to_path_buf());
        let numbers = tokio::task::spawn_blocking(move || {
            let mut numbers = message_numbers(&root)?;
            numbers.extend(message_numbers(&old)?);
            Ok::<_, io::Error>(numbers)
        })
        .await;
        // Without the list, a new message may take a folder; `move_to` refuses to replace one, so
        // the image fails instead of the folder going.
        let numbers = match numbers.map_err(io::Error::other).and_then(|r| r) {
            Ok(numbers) => numbers,
            Err(e) => {
                tracing::error!(error = %e, "couldn't list the attachments folders to reserve their numbers");
                return;
            }
        };
        let reserved = db
            .call(move |_| {
                qm_store::ids::reserve("messages", numbers);
                Ok(())
            })
            .await;
        if let Err(e) = reserved {
            tracing::error!(error = %e, "couldn't reserve the attachments' message numbers");
        }
    }

    /// Moves the previous layout into the data directory, once at startup. Its images were in
    /// the scratch root (`legacy`, `<scratch>/attachments`) and its drafts' in `legacy_drafts`;
    /// an earlier layout's links to them may still sit here. A `message-<id>` folder that open
    /// work names moves here, and a link is left at its old path, so the names already in that
    /// work still open; every other old entry goes. The drafts' images move whole (the drafts in
    /// `ui.json` name them by token, not by path).
    pub async fn migrate_legacy(&self, db: &Db, legacy: &Path, legacy_drafts: &Path) {
        if ensure_scratch_folder(&self.root).is_err() {
            return;
        }
        let (old, root) = (legacy.to_path_buf(), self.root.clone());
        let names = tokio::task::spawn_blocking(move || {
            // An earlier layout's links point from here into the scratch root: only the links go.
            for name in entry_names(&root)? {
                let entry = root.join(&name);
                if fs::symlink_metadata(&entry).is_ok_and(|m| m.file_type().is_symlink()) {
                    qm_types::data_dir::remove_path(&entry)?;
                }
            }
            entry_names(&old)
        })
        .await;
        let names = match names.map_err(io::Error::other).and_then(|r| r) {
            Ok(names) => names,
            Err(e) => {
                tracing::error!(error = %e, "couldn't read the old attachments folder");
                return;
            }
        };
        self.migrate_drafts(legacy_drafts).await;
        if names.is_empty() {
            return;
        }
        let needles: Vec<String> = names
            .iter()
            .filter(|n| n.starts_with(FOLDER_PREFIX))
            .map(|n| format!("/{n}/"))
            .collect();
        let named = db
            .call(move |conn| {
                needles
                    .iter()
                    .map(|needle| qm_store::attachments::in_open_work(conn, needle))
                    .collect::<Result<Vec<bool>, StoreError>>()
            })
            .await;
        let named = match named {
            Ok(named) => named,
            Err(e) => {
                tracing::error!(error = %e, "couldn't tell which old images open work names");
                return;
            }
        };
        let open: BTreeSet<String> = names
            .iter()
            .filter(|n| n.starts_with(FOLDER_PREFIX))
            .zip(named)
            .filter(|(_, named)| *named)
            .map(|(n, _)| n.clone())
            .collect();
        let (old, new) = (legacy.to_path_buf(), self.root.clone());
        let _ = tokio::task::spawn_blocking(move || {
            if ensure_scratch_folder(&old).is_err() {
                return;
            }
            let mut moved = 0_usize;
            for name in names {
                let entry = old.join(&name);
                let link = fs::symlink_metadata(&entry).is_ok_and(|m| m.file_type().is_symlink());
                // A link left by an earlier start stays while open work still names its folder.
                if link && open.contains(&name) {
                    continue;
                }
                if open.contains(&name) && !link {
                    match migrate_directory(&entry, &new.join(&name)) {
                        Ok(()) => moved += 1,
                        Err(e) => tracing::error!(error = %e, "old image folder preserved after migration failure"),
                    }
                } else {
                    let _ = qm_types::data_dir::remove_path(&entry);
                }
            }
            if moved > 0 {
                tracing::info!(moved, "moved pasted images open work names into the data directory");
            }
        })
        .await;
    }

    /// Moves the drafts' images from `legacy` (the scratch root) to the data directory's
    /// `drafts/`, unless that already holds some; the old folder goes either way.
    async fn migrate_drafts(&self, legacy: &Path) {
        let Some(data) = self.root.parent().map(Path::to_path_buf) else {
            return;
        };
        let (old, new) = (
            legacy.to_path_buf(),
            data.join(qm_types::data_dir::DRAFT_IMAGES),
        );
        let _ = tokio::task::spawn_blocking(move || {
            match fs::symlink_metadata(&old) {
                Ok(m) if m.is_dir() && ensure_scratch_folder(&old).is_ok() => {}
                Ok(m) if m.file_type().is_symlink() => {
                    let _ = qm_types::data_dir::remove_path(&old);
                    return;
                }
                _ => return,
            }
            if fs::symlink_metadata(&new).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = qm_types::data_dir::remove_path(&new);
            }
            if fs::symlink_metadata(&new).is_err() {
                if let Err(e) = migrate_directory(&old, &new) {
                    tracing::error!(error = %e, "your drafts' images stay in the old folder");
                    return;
                }
                tracing::info!("moved your drafts' images into the data directory");
            }
            // Drafts name their images by token, so the old path needs no link.
            let _ = qm_types::data_dir::remove_path(&old);
        })
        .await;
    }

    /// Runs the startup pass, then [`Attachments::sweep`] every [`SWEEP_EVERY`], for as long as the
    /// runtime runs.
    pub fn spawn_sweeper(self, db: Db) {
        tokio::spawn(async move {
            self.sweep_folders(&db, true).await;
            loop {
                self.clock.sleep(SWEEP_EVERY).await;
                self.sweep(&db).await;
            }
        });
    }

    /// Applies the cleanup policy (in this module's docs) once.
    pub async fn sweep(&self, db: &Db) {
        self.sweep_folders(db, false).await;
    }

    /// Applies the cleanup at a start, when nothing is staged: every folder open work doesn't
    /// reference goes, whatever its age.
    async fn sweep_folders(&self, db: &Db, startup: bool) {
        if ensure_scratch_folder(&self.root).is_err() {
            return;
        }
        let root = self.root.clone();
        let mut folders = match tokio::task::spawn_blocking(move || list(&root)).await {
            Ok(Ok(folders)) => folders,
            Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => return,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "couldn't list the attachments folder");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "listing the attachments folder stopped");
                return;
            }
        };
        let asked: Vec<(usize, MessageId, String)> = folders
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.message.map(|m| (i, m, format!("/{}/", folder_name(m)))))
            .collect();
        let facts = db
            .call(move |conn| {
                let mut out = Vec::with_capacity(asked.len());
                for (i, message, needle) in asked {
                    // The message must name the folder, or the folder is a leftover whose ID was
                    // reused.
                    let named = messages::text(conn, message)?.is_some_and(|t| t.contains(&needle));
                    let (sent, in_chat) = if named {
                        (
                            qm_store::attachments::sent_at(conn, message)?,
                            qm_store::attachments::in_live_chat(conn, message)?,
                        )
                    } else {
                        (None, false)
                    };
                    let in_work = qm_store::attachments::in_open_work(conn, &needle)?;
                    out.push((i, sent, in_work, in_chat));
                }
                Ok(out)
            })
            .await;
        let facts = match facts {
            Ok(facts) => facts,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't read what the attachments belong to");
                return;
            }
        };
        for (i, sent, in_work, in_chat) in facts {
            folders[i].sent = sent;
            folders[i].in_work = in_work;
            folders[i].in_chat = in_chat;
        }
        let chosen = if startup {
            unreferenced(&folders)
        } else {
            plan(&folders, self.clock.now())
        };
        let doomed: Vec<(PathBuf, u64)> = chosen
            .into_iter()
            .map(|i| (folders[i].path.clone(), folders[i].bytes))
            .collect();
        if doomed.is_empty() {
            return;
        }
        let removed = tokio::task::spawn_blocking(move || {
            let mut count = 0_usize;
            let mut bytes = 0_u64;
            for (path, size) in doomed {
                if path.parent().is_none_or(|p| ensure_scratch_folder(p).is_err()) { continue; }
                match qm_types::data_dir::remove_path(&path) {
                    Ok(()) => {
                        count += 1;
                        bytes += size;
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "couldn't remove an attachment folder");
                    }
                }
            }
            (count, bytes)
        })
        .await;
        if let Ok((count, bytes)) = removed
            && count > 0
        {
            tracing::info!(count, bytes, "removed attachment folders no longer needed");
        }
    }
}

/// Images written to a staging folder, waiting for their message's ID.
#[derive(Debug)]
pub struct Staged {
    root: PathBuf,
    /// The staging folder, until it is renamed (or removed when this is dropped).
    dir: Option<PathBuf>,
    /// One per image, by number: its file name, or why it couldn't be saved.
    images: Vec<(u32, Result<String, String>)>,
}

impl Staged {
    /// Nothing to attach.
    pub fn none(root: PathBuf) -> Self {
        Self {
            root,
            dir: None,
            images: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// In the transaction that stored message `id` with `text`: moves the images to
    /// `message-<id>/` and stores the text with one `[Image #N]: <path>` line per image. Returns
    /// the text as stored, for the copies of your words made in the same transaction.
    pub fn attach(
        &mut self,
        conn: &Connection,
        id: MessageId,
        text: &str,
    ) -> Result<String, StoreError> {
        if self.images.is_empty() {
            return Ok(text.to_owned());
        }
        let folder = self.root.join(folder_name(id));
        if let Some(dir) = self.dir.take()
            && let Err(e) = move_to(&dir, &folder)
        {
            tracing::error!(error = %e, "couldn't move your pasted images into place");
            self.dir = Some(dir);
            let why = format!("couldn't be saved: {e}");
            for (_, saved) in &mut self.images {
                if saved.is_ok() {
                    *saved = Err(why.clone());
                }
            }
        }
        let mut out = text.trim_end().to_owned();
        out.push('\n');
        for (number, saved) in &self.images {
            let placeholder = qm_types::api::image_placeholder(*number);
            match saved {
                Ok(file) => {
                    let _ = write!(out, "\n{placeholder}: {}", folder.join(file).display());
                }
                Err(why) => {
                    let _ = write!(out, "\n{placeholder}: ({why})");
                }
            }
        }
        messages::set_text(conn, id, &out)?;
        // The recipient's conversation now holds these images: they stay while it does.
        qm_store::attachments::record_use_by_recipient(conn, id)?;
        Ok(out)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take()
            && ensure_scratch_folder(&self.root).is_ok()
        {
            let _ = qm_types::data_dir::remove_path(&dir);
        }
    }
}

/// Renames the staging folder to the message's. An existing folder of that name is never
/// replaced: its images may belong to open work whose message is gone.
fn move_to(dir: &Path, folder: &Path) -> io::Result<()> {
    ensure_scratch_folder(
        dir.parent()
            .ok_or_else(|| io::Error::other("missing parent"))?,
    )?;
    ensure_scratch_folder(dir)?;
    if fs::symlink_metadata(folder).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a folder of that name is already here",
        ));
    }
    fs::rename(dir, folder)
}

/// The entries of `dir` by name; none if it doesn't exist.
fn entry_names(dir: &Path) -> io::Result<Vec<String>> {
    if fs::symlink_metadata(dir).is_ok_and(|m| !m.is_dir() || m.file_type().is_symlink()) {
        return Err(io::Error::other("not a plain directory"));
    }
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The message numbers named by the `message-<id>` folders in `root`.
fn message_numbers(root: &Path) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for name in entry_names(root)? {
        let Some(id) = name.strip_prefix(FOLDER_PREFIX) else {
            continue;
        };
        if let Ok(n) = id.parse::<u32>()
            && n.to_string() == id
        {
            out.push(n);
        }
    }
    Ok(out)
}

/// Writes `images` into `root/name/`, each `image-N.<ext>`, private to you (0600).
fn write_staged(root: PathBuf, name: &str, images: Vec<PastedImage>) -> Staged {
    let dir = root.join(name);
    let made =
        ensure_scratch_folder(&root).and_then(|()| fs::DirBuilder::new().mode(0o700).create(&dir));
    let mut staged = Staged {
        root,
        dir: made.is_ok().then(|| dir.clone()),
        images: Vec::with_capacity(images.len()),
    };
    for image in images {
        let saved = match &made {
            Err(e) => Err(format!("couldn't be saved: {e}")),
            Ok(()) => save(&dir, &image),
        };
        staged.images.push((image.number, saved));
    }
    staged
}

fn save(dir: &Path, image: &PastedImage) -> Result<String, String> {
    let bytes = image.bytes.as_slice();
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "not saved: larger than {} MB",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    if qm_types::api::ImageFormat::sniff(bytes) != Some(image.format) {
        return Err(String::from(
            "not saved: not a PNG, JPEG, GIF or WebP image",
        ));
    }
    let file = image.file_name();
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(&file))
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()));
    match written {
        Ok(()) => Ok(file),
        Err(e) => Err(format!("couldn't be saved: {e}")),
    }
}

/// A folder in `attachments/`, with what the sweep learns about it.
#[derive(Clone, Debug)]
struct Folder {
    path: PathBuf,
    /// The message it belongs to (`message-<id>`); `None` for a staging folder.
    message: Option<MessageId>,
    bytes: u64,
    /// When it was last written, from the file system.
    written: Option<jiff::Timestamp>,
    /// When its message was sent, if the message exists and names the folder.
    sent: Option<jiff::Timestamp>,
    /// Whether open work mentions it.
    in_work: bool,
    /// Whether an agent that isn't retired holds it in its current conversation.
    in_chat: bool,
}

/// The staging and message folders in `root`; anything else is skipped.
fn list(root: &Path) -> io::Result<Vec<Folder>> {
    ensure_scratch_folder(root)?;
    let mut out = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let message = if name.starts_with(STAGING_PREFIX) {
            None
        } else if let Some(id) = name.strip_prefix(FOLDER_PREFIX) {
            match id.parse::<i64>() {
                Ok(n) if n > 0 && id == n.to_string() => Some(MessageId::new(n)),
                _ => continue,
            }
        } else {
            continue;
        };
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_dir() {
            continue;
        }
        let written = meta
            .modified()
            .ok()
            .and_then(|t| jiff::Timestamp::try_from(t).ok());
        out.push(Folder {
            path: entry.path(),
            message,
            bytes: size_of(&entry.path()),
            written,
            sent: None,
            in_work: false,
            in_chat: false,
        });
    }
    Ok(out)
}

/// The bytes the files directly in `dir` hold.
fn size_of(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter_map(|e| fs::symlink_metadata(e.path()).ok())
            .filter(fs::Metadata::is_file)
            .map(|m| m.len())
            .sum()
    })
}

/// The folders neither open work nor a live conversation references, by index: all of them, at
/// a start.
fn unreferenced(folders: &[Folder]) -> Vec<usize> {
    (0..folders.len())
        .filter(|&i| !folders[i].in_work && !folders[i].in_chat)
        .collect()
}

/// The folders the policy removes, by index.
fn plan(folders: &[Folder], now: jiff::Timestamp) -> Vec<usize> {
    let age = |at: Option<jiff::Timestamp>| at.map(|at| now.duration_since(at));
    let left_over = |f: &Folder| age(f.written).is_none_or(|a| a >= LEFTOVER_AFTER);
    let mut remove = Vec::new();
    let mut kept: Vec<(usize, jiff::Timestamp)> = Vec::new();
    for (i, f) in folders.iter().enumerate() {
        match (f.message, f.sent) {
            (Some(_), Some(sent)) if f.in_work || f.in_chat => kept.push((i, sent)),
            _ => {
                // Nothing needs it any more: a leftover, or images whose conversation and work
                // are done.
                if !f.in_work && left_over(f) {
                    remove.push(i);
                }
            }
        }
    }
    let mut total: u64 = kept.iter().map(|&(i, _)| folders[i].bytes).sum();
    kept.sort_by_key(|&(_, sent)| sent);
    for (i, sent) in kept {
        if total <= SIZE_CAP || now.duration_since(sent) < ALWAYS_KEEP {
            break;
        }
        // Open work keeps its images, whatever the cap says.
        if folders[i].in_work {
            continue;
        }
        remove.push(i);
        total = total.saturating_sub(folders[i].bytes);
    }
    remove
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const DAY: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

    fn now() -> jiff::Timestamp {
        "2026-10-20T12:00:00Z".parse().unwrap()
    }

    fn ago(by: jiff::SignedDuration) -> jiff::Timestamp {
        now() - by
    }

    /// A message's folder sent `age` ago, `bytes` big.
    fn folder(id: i64, age: jiff::SignedDuration, in_work: bool, bytes: u64) -> Folder {
        Folder {
            path: PathBuf::from(format!("/a/message-{id}")),
            message: Some(MessageId::new(id)),
            bytes,
            written: Some(ago(age)),
            sent: Some(ago(age)),
            in_work,
            in_chat: !in_work,
        }
    }

    #[test]
    fn images_no_conversation_or_open_work_holds_go_after_an_hour() {
        let mut done = folder(1, jiff::SignedDuration::from_hours(2), false, 1);
        done.in_chat = false;
        let mut recent = folder(2, jiff::SignedDuration::from_mins(20), false, 1);
        recent.in_chat = false;
        let chat = folder(3, 30 * DAY, false, 1);
        let work = folder(4, 30 * DAY, true, 1);
        assert_eq!(plan(&[done, recent, chat, work], now()), vec![0]);
    }

    #[test]
    fn an_old_image_stays_while_the_folder_is_under_the_cap() {
        let folders = [
            folder(1, 15 * DAY, false, 10),
            folder(2, 15 * DAY, true, 10),
            folder(3, 3 * DAY, false, 10),
        ];
        assert_eq!(plan(&folders, now()), Vec::<usize>::new());
    }

    #[test]
    fn restart_orphans_in_open_work_survive_hourly_sweeping_even_over_cap() {
        let mut referenced = folder(7, 5 * DAY, true, SIZE_CAP + 1);
        referenced.sent = None;
        assert_eq!(
            plan(std::slice::from_ref(&referenced), now()),
            Vec::<usize>::new()
        );
        assert_eq!(
            unreferenced(std::slice::from_ref(&referenced)),
            Vec::<usize>::new()
        );
        referenced.in_work = false;
        referenced.in_chat = false;
        assert_eq!(plan(&[referenced], now()), vec![0]);
    }

    #[test]
    fn staging_does_not_follow_a_linked_scratch_ancestor() {
        let root = Root::new("symlink-stage");
        let victim = root.0.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("unrelated"), b"safe").unwrap();
        let link = root.0.join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let staged = write_staged(link.join("attachments"), ".staging-test", Vec::new());
        assert!(staged.dir.is_none());
        assert!(!victim.join("attachments").exists());
        assert_eq!(fs::read(victim.join("unrelated")).unwrap(), b"safe");
    }

    #[test]
    fn leftovers_go_after_an_hour() {
        let mut staging = folder(0, jiff::SignedDuration::from_mins(90), false, 1);
        staging.message = None;
        let mut fresh = staging.clone();
        fresh.written = Some(ago(jiff::SignedDuration::from_mins(5)));
        // A folder whose message doesn't name it.
        let mut orphan = folder(7, 2 * DAY, false, 1);
        orphan.sent = None;
        let folders = [staging, fresh, orphan];
        assert_eq!(plan(&folders, now()), vec![0, 2]);
    }

    #[test]
    fn over_the_cap_the_oldest_go_first_but_never_todays() {
        let third = SIZE_CAP / 3 + 1;
        let folders = [
            folder(1, 5 * DAY, true, third),
            folder(2, 3 * DAY, false, third),
            folder(3, jiff::SignedDuration::from_hours(2), false, third),
        ];
        // Over the cap: the oldest folder open work mentions is kept, so the next one goes.
        assert_eq!(plan(&folders, now()), vec![1]);
        let today = [
            folder(1, jiff::SignedDuration::from_hours(3), false, SIZE_CAP),
            folder(2, jiff::SignedDuration::from_hours(2), false, SIZE_CAP),
        ];
        assert_eq!(plan(&today, now()), Vec::<usize>::new());
    }

    #[test]
    fn the_draft_images_folder_is_never_listed_for_removal() {
        let root = std::env::temp_dir().join(format!(
            "qm-attachments-list-{}-{}",
            std::process::id(),
            NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
        ));
        let drafts = root.join(qm_types::data_dir::DRAFT_IMAGES);
        fs::create_dir_all(&drafts).unwrap();
        fs::write(drafts.join("0123456789abcdef-4.png"), b"x").unwrap();
        fs::create_dir_all(root.join("message-5")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let listed = list(&root).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].message, Some(MessageId::new(5)));
    }

    /// A scratch attachments folder for one test, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "qm-attachments-{name}-{}-{}",
                std::process::id(),
                NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
            ));
            qm_types::data_dir::private_folder(&root).expect("a scratch folder");
            Self(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn attachment_sweep_does_not_follow_a_message_directory_symlink() {
        let root = Root::new("sweep-link");
        let victim = root.0.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("unrelated"), b"safe").unwrap();
        std::os::unix::fs::symlink(&victim, root.0.join("message-7")).unwrap();
        assert_eq!(list(&root.0).unwrap().len(), 0);
        assert_eq!(fs::read(victim.join("unrelated")).unwrap(), b"safe");
    }

    #[test]
    fn a_folder_is_never_replaced_by_a_new_message() {
        let root = Root::new("replace");
        let folder = root.0.join(folder_name(MessageId::new(7)));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("image-1.png"), b"open work's").unwrap();
        let staging = root.0.join(format!("{STAGING_PREFIX}1"));
        qm_types::data_dir::private_folder(&staging).unwrap();

        assert!(move_to(&staging, &folder).is_err(), "the folder is taken");
        assert_eq!(
            fs::read(folder.join("image-1.png")).unwrap(),
            b"open work's",
            "the folder's images are untouched"
        );
    }

    #[test]
    fn the_message_numbers_on_disk_are_the_folders_that_name_one() {
        let root = Root::new("numbers");
        for name in [
            "message-7",
            "message-42",
            "message-07",
            "message-x",
            ".staging-1",
        ] {
            fs::create_dir_all(root.0.join(name)).unwrap();
        }
        let mut numbers = message_numbers(&root.0).unwrap();
        numbers.sort_unstable();
        assert_eq!(numbers, vec![7, 42]);
    }

    #[tokio::test]
    async fn an_old_folder_open_work_names_moves_and_the_rest_goes() {
        let root = Root::new("legacy");
        let (old, drafts, new) = (
            root.0.join("old"),
            root.0.join("drafts"),
            root.0.join("scratch"),
        );
        let named = old.join("message-7");
        fs::create_dir_all(&named).unwrap();
        fs::write(named.join("image-1.png"), b"kept").unwrap();
        fs::create_dir_all(old.join("message-8")).unwrap();
        fs::create_dir_all(&drafts).unwrap();
        fs::write(drafts.join("0123456789abcdef-1.png"), b"draft").unwrap();

        let db = Db::open(&root.0.join("state.db")).expect("a database");
        let goal = format!("look at {}/image-1.png", named.display());
        db.call(move |conn| {
            conn.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                     merge_method, created_at, seq) \
                 VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
                 INSERT INTO agents (id, role, home, status, created_at, seq) \
                 VALUES (1, 'quartermaster', '/home', 'idle', 1, 1);",
            )?;
            conn.execute(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, \
                     delivery_mode, branch, status, created_at, seq) \
                 VALUES (1, 1, 1, 'build', 't', ?1, 'direct_pr', 'qm/task-1', 'running', 1, 1)",
                [goal],
            )?;
            Ok(())
        })
        .await
        .expect("the open task");

        let attachments = Attachments::new(new.clone(), Arc::new(qm_types::clock::SystemClock));
        ensure_scratch_folder(&new).unwrap();
        fs::create_dir(new.join("message-7")).unwrap();
        attachments.migrate_legacy(&db, &old, &drafts).await;
        assert_eq!(fs::read(named.join("image-1.png")).unwrap(), b"kept");
        assert!(!new.join("message-7/image-1.png").exists());
        fs::remove_dir(new.join("message-7")).unwrap();
        attachments.migrate_legacy(&db, &old, &drafts).await;

        assert_eq!(
            fs::read(new.join("message-7/image-1.png")).unwrap(),
            b"kept"
        );
        assert!(
            fs::symlink_metadata(&named)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the old path links to the moved folder, so the names in open work still open"
        );
        assert_eq!(fs::read(named.join("image-1.png")).unwrap(), b"kept");
        assert!(!old.join("message-8").exists(), "an unnamed folder goes");
        assert!(!drafts.exists(), "old drafts go");
    }

    #[tokio::test]
    async fn a_number_named_on_disk_is_never_issued_to_a_new_message() {
        let root = Root::new("reserve");
        fs::create_dir_all(root.0.join("message-8")).unwrap();
        let db_path = root.0.join("state.db");
        let db = Db::open(&db_path).expect("a database");
        let attachments = Attachments::new(root.0.clone(), Arc::new(qm_types::clock::SystemClock));
        attachments
            .reserve_numbers(&db, &root.0.join("legacy"))
            .await;
        db.call(|conn| {
            for _ in 0..50 {
                let n = qm_store::ids::fresh(conn, "messages", 8..=9)?;
                assert_eq!(n, 9, "8 belongs to a folder on disk");
            }
            Ok(())
        })
        .await
        .expect("the numbers");
    }
}
