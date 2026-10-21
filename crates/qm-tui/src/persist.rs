//! What the UI remembers across a server restart: `<data dir>/ui.json`.
//!
//! The host (`host`) owns the one [`App`], so it owns this too. It saves the state every window
//! shares and that survives a restart: the screen, the popups and their order, focus, the crew and
//! project selection, the scrolls, the column widths, the Settings sub-tab, the drafts (text,
//! cursor, selection and pasted-image tokens) and which tool-call runs are expanded. Never the
//! per-window state (`Term`), a context menu, a picker, or a text field in Settings: an API key
//! being typed is never written.
//!
//! - **When.** [`Persister::tick`] notices a change and writes [`DEBOUNCE`] after the last one, so
//!   typing is one write a second at most. [`Persister::save`] writes at once: when a window
//!   detaches, and when the host stops ([`Persister::flush`] waits for it).
//! - **How.** A writer thread takes each snapshot as text: a temporary file at mode 0600, `fsync`,
//!   a rename over `ui.json`, and `fsync` of the directory. The host never waits for the disk,
//!   except at stop.
//! - **Loading never fails.** A missing, unreadable or odd file gives the defaults; each field is
//!   read on its own, so one bad value spoils only itself; unknown fields and versions are read as
//!   far as they make sense. With no `ui.json`, the column widths come from `tui.toml`
//!   (`input::prefs`). Things that need live state to be right (the Setup and Diff screens, a
//!   session, a context menu) come back as the Bridge.
//! - **Images.** A draft's pasted images are written once to `attachments/drafts/<token>.<ext>`
//!   (the token is a hash of the bytes and their length) and `ui.json` names the token. After each
//!   write, files no saved draft names are removed. The core's cleanup leaves the folder alone
//!   (`qm_core::attachments`), so an image in an unsent draft outlives a restart.
//! - **Never logged.** It holds your unsent words; errors are dropped, not reported.

use crate::app::{App, LEFT_RANGE, RIGHT_RANGE};
use crate::hit::Pane;
use crate::input::draft::Draft;
use crate::input::prefs;
use crate::popups::{OpenPopup, Popup};
use crate::router::Screen;
use crate::session::{SessionUi, ToolGroupKey};
use crate::settings::{ROUTING_ROWS, Tab};
use qm_types::api::{AgentRef, ImageFormat, MAX_IMAGE_BYTES, PastedImage};
use qm_types::data_dir::DataDir;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The file's name in the data directory.
pub(crate) const FILE: &str = "ui.json";

/// The format version; bumped only when an old reader would misread a new file.
const VERSION: u32 = 1;

/// How long after the last change the state is written.
pub(crate) const DEBOUNCE: Duration = Duration::from_secs(1);

/// How long a stop waits for the last write.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

/// A scroll offset past this is not a real one.
const MAX_SCROLL: usize = 1_000_000;

/// The most popups, drafts and expanded runs read back.
const MAX_LIST: usize = 256;

// ---------------------------------------------------------------------------------------------
// What is written

/// A pasted image as the file names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ImageRef {
    number: u32,
    format: ImageFormat,
    len: usize,
    /// Names its file in `attachments/drafts/`; empty while only comparing states.
    token: String,
}

/// A draft, with its pasted images by token.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct DraftState {
    text: String,
    cursor: Option<usize>,
    anchor: Option<usize>,
    images: Vec<ImageRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SessionDraft {
    agent: AgentRef,
    draft: DraftState,
}

/// How far each scrolling part is scrolled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct Scrolls {
    conversation: usize,
    projects: usize,
    crew: usize,
    task: usize,
    diff: usize,
    help: usize,
    settings: usize,
}

/// Everything that is written, in file order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct State {
    version: u32,
    screen: Screen,
    popups: Vec<OpenPopup>,
    focus: Pane,
    selected_crew: Option<AgentRef>,
    selected_project: Option<usize>,
    scroll: Scrolls,
    left_width: u16,
    right_width: u16,
    settings_tab: Tab,
    routing_row: usize,
    bridge_draft: DraftState,
    session_drafts: Vec<SessionDraft>,
    tool_groups: Vec<ToolGroupKey>,
}

/// An image file to write: its name in `attachments/drafts/` and what is in it.
type ImageFile<'a> = (String, &'a [u8]);

impl State {
    /// What `app` holds now. With `files`, images get their tokens and the files they need are
    /// listed; without, they are described by number, format and size only (cheap, for noticing
    /// changes).
    fn of<'a>(app: &'a App, files: Option<&mut Vec<ImageFile<'a>>>) -> Self {
        let mut files = files;
        let mut draft = |draft: &'a Draft| draft_state(draft, files.as_deref_mut());
        let ui = app.view.session.as_ref();
        // While a session shows, its draft is the app's and the Quartermaster's is parked.
        let (bridge, shown) = match app.shown_session() {
            Some(agent) => (
                ui.map(|ui| &ui.bridge_draft),
                Some(SessionDraft {
                    agent,
                    draft: draft(&app.draft),
                }),
            ),
            None => (Some(&app.draft), None),
        };
        let bridge_draft = bridge.map(&mut draft).unwrap_or_default();
        let shown_agent = shown.as_ref().map(|s| s.agent);
        let mut session_drafts: Vec<SessionDraft> = ui
            .into_iter()
            .flat_map(|ui| &ui.drafts)
            .filter(|(agent, _)| shown_agent != Some(**agent))
            .filter(|(_, d)| !d.is_empty())
            .map(|(agent, d)| SessionDraft {
                agent: *agent,
                draft: draft(d),
            })
            .chain(shown)
            .collect();
        session_drafts.sort_by_cached_key(|s| format!("{:?}", s.agent));
        let mut tool_groups: Vec<ToolGroupKey> = app.view.tool_groups.iter().copied().collect();
        tool_groups.sort_by_cached_key(|key| format!("{key:?}"));
        Self {
            version: VERSION,
            screen: app.screen,
            popups: app.view.popups.clone(),
            focus: app.focus,
            selected_crew: app.view.selected_crew,
            selected_project: app.view.selected_project,
            scroll: Scrolls {
                conversation: app.scroll_from_bottom,
                projects: app.view.projects_scroll,
                crew: app.view.crew_scroll,
                task: app.view.task_scroll,
                diff: app.view.diff_scroll,
                help: app.view.help_scroll,
                settings: app.view.settings.scroll,
            },
            left_width: app.left_width,
            right_width: app.right_width,
            settings_tab: app.view.settings.tab,
            routing_row: app.view.settings.routing_row,
            bridge_draft,
            session_drafts,
            tool_groups,
        }
    }
}

fn draft_state<'a>(draft: &'a Draft, mut files: Option<&mut Vec<ImageFile<'a>>>) -> DraftState {
    let images = draft
        .images
        .iter()
        .filter(|image| draft.text.contains(&image.placeholder()))
        .map(|image| {
            let bytes = image.bytes.as_slice();
            let mut token = String::new();
            if let Some(files) = files.as_deref_mut() {
                token = token_of(bytes);
                files.push((file_name(&token, image.format), bytes));
            }
            ImageRef {
                number: image.number,
                format: image.format,
                len: bytes.len(),
                token,
            }
        })
        .collect();
    DraftState {
        text: draft.text.clone(),
        cursor: draft.cursor,
        anchor: draft.anchor,
        images,
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

// ---------------------------------------------------------------------------------------------
// What is read

/// What `ui.json` held, as values the app takes.
#[derive(Debug)]
struct Restored {
    popups: Vec<OpenPopup>,
    focus: Pane,
    selected_crew: Option<AgentRef>,
    selected_project: Option<usize>,
    scroll: Scrolls,
    widths: Option<(u16, u16)>,
    settings_tab: Tab,
    routing_row: usize,
    bridge_draft: Draft,
    session_drafts: Vec<(AgentRef, Draft)>,
    tool_groups: Vec<ToolGroupKey>,
    /// The image files its drafts use, so they aren't written again.
    image_files: Vec<String>,
}

fn field<T: DeserializeOwned>(object: &serde_json::Map<String, Value>, key: &str) -> Option<T> {
    serde_json::from_value(object.get(key)?.clone()).ok()
}

/// A list read item by item, so a bad item drops only itself.
fn list<T: DeserializeOwned>(object: &serde_json::Map<String, Value>, key: &str) -> Vec<T> {
    object
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_LIST)
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect()
}

fn scroll(object: Option<&serde_json::Map<String, Value>>, key: &str) -> usize {
    object
        .and_then(|o| field::<usize>(o, key))
        .map_or(0, |n| n.min(MAX_SCROLL))
}

fn width(value: Option<u16>, range: (u16, u16)) -> Option<u16> {
    value.map(|w| w.clamp(range.0, range.1))
}

/// A draft; its images come back only when their files are there and are what they say.
fn read_draft(value: Option<&Value>, images: &Path, files: &mut Vec<String>) -> Draft {
    let Some(object) = value.and_then(Value::as_object) else {
        return Draft::default();
    };
    let mut draft = Draft::new(field::<String>(object, "text").unwrap_or_default());
    draft.cursor = field(object, "cursor");
    draft.anchor = field(object, "anchor");
    for image in list::<Value>(object, "images") {
        let Some(image) = image.as_object() else {
            continue;
        };
        let (Some(number), Some(format), Some(token)) = (
            field::<u32>(image, "number"),
            field::<ImageFormat>(image, "format"),
            field::<String>(image, "token"),
        ) else {
            continue;
        };
        if !token_ok(&token)
            || !draft
                .text
                .contains(&qm_types::api::image_placeholder(number))
        {
            continue;
        }
        let name = file_name(&token, format);
        let path = images.join(&name);
        let big_enough = fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_IMAGE_BYTES as u64);
        let Some(bytes) = big_enough.then(|| fs::read(&path).ok()).flatten() else {
            continue;
        };
        if ImageFormat::sniff(&bytes) != Some(format) {
            continue;
        }
        draft.images.push(PastedImage::new(number, format, bytes));
        files.push(name);
    }
    draft
}

impl Restored {
    /// Reads `text`, which may be anything.
    fn parse(text: &str, images: &Path) -> Self {
        let value: Value = serde_json::from_str(text).unwrap_or_default();
        let empty = serde_json::Map::new();
        let object = value.as_object().unwrap_or(&empty);
        let mut image_files = Vec::new();
        let scrolls = object.get("scroll").and_then(Value::as_object);
        let mut popups: Vec<OpenPopup> = Vec::new();
        for open in list::<OpenPopup>(object, "popups") {
            // One of each, the last one on top.
            popups.retain(|o| o.popup != open.popup);
            popups.push(open);
        }
        let bridge_draft = read_draft(object.get("bridge_draft"), images, &mut image_files);
        let session_drafts = list::<Value>(object, "session_drafts")
            .into_iter()
            .filter_map(|item| {
                let item = item.as_object()?;
                let agent = field::<AgentRef>(item, "agent")?;
                let draft = read_draft(item.get("draft"), images, &mut image_files);
                Some((agent, draft))
            })
            .collect();
        Self {
            popups,
            focus: field(object, "focus").unwrap_or(Pane::Input),
            selected_crew: field(object, "selected_crew"),
            selected_project: field::<usize>(object, "selected_project").filter(|&i| i < MAX_LIST),
            scroll: Scrolls {
                conversation: scroll(scrolls, "conversation"),
                projects: scroll(scrolls, "projects"),
                crew: scroll(scrolls, "crew"),
                task: scroll(scrolls, "task"),
                diff: scroll(scrolls, "diff"),
                help: scroll(scrolls, "help"),
                settings: scroll(scrolls, "settings"),
            },
            widths: width(field(object, "left_width"), LEFT_RANGE)
                .zip(width(field(object, "right_width"), RIGHT_RANGE)),
            settings_tab: field(object, "settings_tab").unwrap_or_default(),
            routing_row: field::<usize>(object, "routing_row")
                .unwrap_or(0)
                .min(ROUTING_ROWS - 1),
            bridge_draft,
            session_drafts,
            tool_groups: list(object, "tool_groups"),
            image_files,
        }
    }

    /// Puts it into `app`. The screen stays the Bridge: the Setup and Diff screens need live
    /// state to show anything, and a session opens again from its crew row.
    fn apply(self, app: &mut App) {
        if let Some((left, right)) = self.widths {
            app.left_width = left;
            app.right_width = right;
        }
        app.view.popups = self.popups;
        // A pane that only exists in a popup needs its popup.
        let has = |popup| app.view.popups.iter().any(|o| o.popup == popup);
        app.focus = match self.focus {
            Pane::Settings if !has(Popup::Settings) => Pane::Input,
            Pane::Help if !has(Popup::Help) => Pane::Input,
            Pane::Session => Pane::Input,
            pane => pane,
        };
        app.view.selected_crew = self.selected_crew;
        app.view.selected_project = self.selected_project;
        app.scroll_from_bottom = self.scroll.conversation;
        app.view.projects_scroll = self.scroll.projects;
        app.view.crew_scroll = self.scroll.crew;
        app.view.task_scroll = self.scroll.task;
        app.view.diff_scroll = self.scroll.diff;
        app.view.help_scroll = self.scroll.help;
        app.view.settings.scroll = self.scroll.settings;
        app.view.settings.tab = self.settings_tab;
        app.view.settings.routing_row = self.routing_row;
        app.draft = self.bridge_draft;
        let ui = app.view.session.get_or_insert_with(SessionUi::default);
        ui.drafts = self
            .session_drafts
            .into_iter()
            .filter(|(_, draft)| !draft.is_empty())
            .collect();
        app.view.tool_groups = self.tool_groups.into_iter().collect();
        app.redraw();
    }
}

// ---------------------------------------------------------------------------------------------
// Writing

/// One snapshot for the writer thread.
struct Job {
    json: String,
    /// Image files not written before, with their bytes.
    images: Vec<(String, Vec<u8>)>,
    /// Every image file the snapshot names; the rest of the folder goes.
    keep: HashSet<String>,
    /// Told when it is on disk.
    done: Option<mpsc::Sender<()>>,
}

/// Where the state lives.
#[derive(Clone, Debug)]
struct Paths {
    file: PathBuf,
    images: PathBuf,
    legacy: PathBuf,
}

impl Paths {
    fn of(dir: &DataDir) -> Self {
        Self {
            file: dir.root().join(FILE),
            images: dir.draft_images(),
            legacy: dir.root().join(prefs::FILE),
        }
    }
}

/// Remembers the app's state: loads it at start, and saves it as it changes.
#[derive(Debug)]
pub(crate) struct Persister {
    jobs: Option<mpsc::Sender<Job>>,
    writer: Option<JoinHandle<()>>,
    /// What was last seen (image bytes not included), to notice changes.
    seen: State,
    /// When the state last changed, until it is written.
    changed: Option<Instant>,
    /// Image files already written (or loaded) this run.
    written: HashSet<String>,
}

impl Persister {
    /// Loads the saved state into `app` and starts remembering it.
    pub(crate) fn open(dir: &DataDir, app: &mut App) -> Self {
        let paths = Paths::of(dir);
        let saved = fs::read_to_string(&paths.file).ok();
        let mut restored = Restored::parse(saved.as_deref().unwrap_or("{}"), &paths.images);
        if restored.widths.is_none() {
            // Nothing saved here yet: the widths older versions kept.
            let old = prefs::load(&paths.legacy);
            restored.widths = Some((old.left_width, old.right_width));
        }
        let written = restored.image_files.iter().cloned().collect();
        restored.apply(app);
        let (jobs, queue) = mpsc::channel::<Job>();
        let (file, images) = (paths.file.clone(), paths.images.clone());
        let writer = std::thread::Builder::new()
            .name("ui-state".to_owned())
            .spawn(move || {
                for job in queue {
                    write_job(&file, &images, job);
                }
            })
            .ok();
        Self {
            jobs: writer.is_some().then_some(jobs),
            writer,
            seen: State::of(app, None),
            changed: None,
            written,
        }
    }

    /// Notices a change in `app` and writes it once it has been quiet for [`DEBOUNCE`].
    pub(crate) fn tick(&mut self, app: &App, now: Instant) {
        if app.view.sample {
            return;
        }
        let state = State::of(app, None);
        if state != self.seen {
            self.seen = state;
            self.changed = Some(now);
        }
        if self
            .changed
            .is_some_and(|at| now.saturating_duration_since(at) >= DEBOUNCE)
        {
            self.save(app);
        }
    }

    /// Writes `app`'s state now, without waiting for the disk.
    pub(crate) fn save(&mut self, app: &App) {
        let _ = self.submit(app, None);
    }

    /// Writes `app`'s state now and waits (a little) for the disk: for when the host stops.
    pub(crate) fn flush(&mut self, app: &App) {
        let (done, wait) = mpsc::channel();
        if self.submit(app, Some(done)) {
            let _ = wait.recv_timeout(FLUSH_WAIT);
        }
    }

    fn submit(&mut self, app: &App, done: Option<mpsc::Sender<()>>) -> bool {
        if app.view.sample {
            return false;
        }
        let Some(jobs) = &self.jobs else {
            return false;
        };
        let mut files = Vec::new();
        let state = State::of(app, Some(&mut files));
        let Ok(json) = serde_json::to_string_pretty(&state) else {
            return false;
        };
        let keep = files.iter().map(|(name, _)| name.clone()).collect();
        let images = files
            .into_iter()
            .filter(|(name, _)| !self.written.contains(name))
            .map(|(name, bytes)| (name, bytes.to_vec()))
            .collect::<Vec<_>>();
        let new: Vec<String> = images.iter().map(|(name, _)| name.clone()).collect();
        let sent = jobs
            .send(Job {
                json,
                images,
                keep,
                done,
            })
            .is_ok();
        if sent {
            self.written.extend(new);
            self.seen = State::of(app, None);
            self.changed = None;
        }
        sent
    }
}

impl Drop for Persister {
    /// Lets the writer finish what is queued.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Writes one snapshot: new images, then `ui.json`, then removes the images nothing names.
fn write_job(file: &Path, images: &Path, job: Job) {
    let mut ok = true;
    if !job.images.is_empty() {
        ok = make_dir(images).is_ok();
        for (name, bytes) in &job.images {
            let path = images.join(name);
            if ok && !path.exists() {
                ok = write_atomic(&path, bytes).is_ok();
            }
        }
    }
    if ok && write_atomic(file, job.json.as_bytes()).is_ok() {
        remove_unnamed(images, &job.keep);
    }
    if let Some(done) = job.done {
        let _ = done.send(());
    }
}

fn make_dir(dir: &Path) -> io::Result<()> {
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

/// Writes `bytes` to `path` all at once or not at all: a temporary file at mode 0600, flushed to
/// disk, renamed into place, and the directory flushed too.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
