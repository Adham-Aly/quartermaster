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
//! [Image #1]: /Users/you/.quartermaster/attachments/message-42/image-1.png
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
//! 1. **Over the cap.** While what is left holds more than [`SIZE_CAP`], the oldest folders go
//!    first, except those from the last [`ALWAYS_KEEP`] and those open work mentions: no open
//!    task's goal, words, instructions, constraints, later steps or crewmate messages, and no open
//!    secondmate request. Nothing is removed by age: the folder lives in `/tmp`, which the system's
//!    own cleaner empties once it has gone untouched for days.
//! 2. **Leftovers.** A staging folder, or a `message-<id>` folder whose message doesn't name it,
//!    once its modification time is [`LEFTOVER_AFTER`] old: what a crash between writing and
//!    storing leaves behind.
//!
//! Anything else in the folder is left alone. That includes `drafts/` (`DataDir::draft_images`):
//! the images in your unsent drafts, which the UI host keeps for `ui.json` and removes itself once
//! no saved draft references them. So a draft's image survives the sweep, and a restart.

use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::messages;
use qm_types::api::{MAX_IMAGE_BYTES, PastedImage};
use qm_types::clock::Clock;
use qm_types::ids::MessageId;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
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

    /// Makes the previous layout temporary, once at startup. Its images were in the data
    /// directory (`legacy`, `<data>/attachments`) and its drafts' in `legacy_drafts`. A
    /// `message-<id>` folder that open work names moves here, and a link is left at its old path,
    /// so the names already in that work still open; every other entry goes. Drafts go whole:
    /// drafts are held by the server now, and nothing names the old files.
    pub async fn migrate_legacy(&self, db: &Db, legacy: &Path, legacy_drafts: &Path) {
        let (old, drafts) = (legacy.to_path_buf(), legacy_drafts.to_path_buf());
        let names = tokio::task::spawn_blocking(move || {
            let names = entry_names(&old)?;
            let _ = fs::remove_dir_all(&drafts);
            Ok::<_, io::Error>(names)
        })
        .await;
        let names = match names.map_err(io::Error::other).and_then(|r| r) {
            Ok(names) => names,
            Err(e) => {
                tracing::error!(error = %e, "couldn't read the old attachments folder");
                return;
            }
        };
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
            for name in names {
                let entry = old.join(&name);
                let link = fs::symlink_metadata(&entry).is_ok_and(|m| m.file_type().is_symlink());
                // A link left by an earlier start stays while open work still names its folder.
                if link && open.contains(&name) {
                    continue;
                }
                if open.contains(&name) && !link && !new.join(&name).exists() {
                    let target = new.join(&name);
                    if let Err(e) = fs::create_dir_all(&new).and_then(|()| fs::rename(&entry, &target)) {
                        tracing::error!(error = %e, "couldn't move an old image folder that open work names");
                        continue;
                    }
                    if let Err(e) = std::os::unix::fs::symlink(&target, &entry) {
                        tracing::error!(error = %e, "couldn't leave a link to an old image folder");
                    }
                } else {
                    let _ = qm_types::data_dir::remove_path(&entry);
                }
            }
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
                    let sent = if named {
                        qm_store::attachments::sent_at(conn, message)?
                    } else {
                        None
                    };
                    let in_use = qm_store::attachments::in_open_work(conn, &needle)?;
                    out.push((i, sent, in_use));
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
        for (i, sent, in_use) in facts {
            folders[i].sent = sent;
            folders[i].in_use = in_use;
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
                match fs::remove_dir_all(&path) {
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
        Ok(out)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// Renames the staging folder to the message's. An existing folder of that name is never
/// replaced: its images may belong to open work whose message is gone.
fn move_to(dir: &Path, folder: &Path) -> io::Result<()> {
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
    let made = qm_types::data_dir::private_folder(&root).and_then(|()| fs::create_dir_all(&dir));
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
    in_use: bool,
}

/// The staging and message folders in `root`; anything else is skipped.
fn list(root: &Path) -> io::Result<Vec<Folder>> {
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
        let meta = entry.metadata()?;
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
            in_use: false,
        });
    }
    Ok(out)
}

/// The bytes the files directly in `dir` hold.
fn size_of(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .filter(fs::Metadata::is_file)
            .map(|m| m.len())
            .sum()
    })
}

/// The folders no open work references, by index: all of them, at a start.
fn unreferenced(folders: &[Folder]) -> Vec<usize> {
    (0..folders.len()).filter(|&i| !folders[i].in_use).collect()
}

/// The folders the policy removes, by index.
fn plan(folders: &[Folder], now: jiff::Timestamp) -> Vec<usize> {
    let age = |at: Option<jiff::Timestamp>| at.map(|at| now.duration_since(at));
    let left_over = |f: &Folder| age(f.written).is_none_or(|a| a >= LEFTOVER_AFTER);
    let mut remove = Vec::new();
    let mut kept: Vec<(usize, jiff::Timestamp)> = Vec::new();
    for (i, f) in folders.iter().enumerate() {
        match (f.message, f.sent) {
            (None, _) | (Some(_), None) => {
                if left_over(f) {
                    remove.push(i);
                }
            }
            (Some(_), Some(sent)) => kept.push((i, sent)),
        }
    }
    let mut total: u64 = kept.iter().map(|&(i, _)| folders[i].bytes).sum();
    kept.sort_by_key(|&(_, sent)| sent);
    for (i, sent) in kept {
        if total <= SIZE_CAP || now.duration_since(sent) < ALWAYS_KEEP {
            break;
        }
        // Open work keeps its images, whatever the cap says.
        if folders[i].in_use {
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

    const DAY: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

    fn now() -> jiff::Timestamp {
        "2026-10-20T12:00:00Z".parse().unwrap()
    }

    fn ago(by: jiff::SignedDuration) -> jiff::Timestamp {
        now() - by
    }

    /// A message's folder sent `age` ago, `bytes` big.
    fn folder(id: i64, age: jiff::SignedDuration, in_use: bool, bytes: u64) -> Folder {
        Folder {
            path: PathBuf::from(format!("/a/message-{id}")),
            message: Some(MessageId::new(id)),
            bytes,
            written: Some(ago(age)),
            sent: Some(ago(age)),
            in_use,
        }
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
            fs::create_dir_all(&root).expect("a scratch folder");
            Self(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_folder_is_never_replaced_by_a_new_message() {
        let root = Root::new("replace");
        let folder = root.0.join(folder_name(MessageId::new(7)));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("image-1.png"), b"open work's").unwrap();
        let staging = root.0.join(format!("{STAGING_PREFIX}1"));
        fs::create_dir_all(&staging).unwrap();

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
