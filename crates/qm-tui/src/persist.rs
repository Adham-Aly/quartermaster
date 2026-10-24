//! What the UI remembers across a server restart: `<data dir>/ui.json`.
//!
//! The host (`host`) owns the one [`App`], so it owns this too. It saves the state every window
//! shares and that survives a restart: the screen, the popups and their order, focus, the crew and
//! project selection, the scrolls, the column widths, the Settings sub-tab and which tool-call runs
//! are expanded. Never the drafts (`draft_sync`: the core holds them, and they never reach the disk),
//! the per-window state (`Term`), a context menu, a picker, or a text field in Settings: an API key
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
//! - **Never logged.** Errors are dropped, not reported.

use crate::app::{App, LEFT_RANGE, RIGHT_RANGE};
use crate::hit::Pane;
use crate::input::prefs;
use crate::popups::{OpenPopup, Popup};
use crate::router::Screen;
use crate::session::ToolGroupKey;
use crate::settings::{ROUTING_ROWS, Tab};
use qm_types::api::AgentRef;
use qm_types::data_dir::DataDir;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
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

/// The most popups and expanded runs read back.
const MAX_LIST: usize = 256;

// ---------------------------------------------------------------------------------------------
// What is written

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
    tool_groups: Vec<ToolGroupKey>,
}

impl State {
    /// What `app` holds now.
    fn of(app: &App) -> Self {
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
                diff: app.view.diff.scroll,
                help: app.view.help_scroll,
                settings: app.view.settings.scroll,
            },
            left_width: app.left_width,
            right_width: app.right_width,
            settings_tab: app.view.settings.tab,
            routing_row: app.view.settings.routing_row,
            tool_groups,
        }
    }
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
    tool_groups: Vec<ToolGroupKey>,
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

impl Restored {
    /// Reads `text`, which may be anything.
    fn parse(text: &str) -> Self {
        let value: Value = serde_json::from_str(text).unwrap_or_default();
        let empty = serde_json::Map::new();
        let object = value.as_object().unwrap_or(&empty);
        let scrolls = object.get("scroll").and_then(Value::as_object);
        let mut popups: Vec<OpenPopup> = Vec::new();
        for open in list::<OpenPopup>(object, "popups") {
            // One of each, the last one on top.
            popups.retain(|o| o.popup != open.popup);
            popups.push(open);
        }
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
            tool_groups: list(object, "tool_groups"),
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
        app.view.diff.scroll = self.scroll.diff;
        app.view.help_scroll = self.scroll.help;
        app.view.settings.scroll = self.scroll.settings;
        app.view.settings.tab = self.settings_tab;
        app.view.settings.routing_row = self.routing_row;
        app.view.tool_groups = self.tool_groups.into_iter().collect();
        app.redraw();
    }
}

// ---------------------------------------------------------------------------------------------
// Writing

/// One snapshot for the writer thread.
struct Job {
    json: String,
    /// Told when it is on disk.
    done: Option<mpsc::Sender<()>>,
}

/// Where the state lives.
#[derive(Clone, Debug)]
struct Paths {
    file: PathBuf,
    legacy: PathBuf,
}

impl Paths {
    fn of(dir: &DataDir) -> Self {
        Self {
            file: dir.root().join(FILE),
            legacy: dir.root().join(prefs::FILE),
        }
    }
}

/// Remembers the app's state: loads it at start, and saves it as it changes.
#[derive(Debug)]
pub(crate) struct Persister {
    jobs: Option<mpsc::Sender<Job>>,
    writer: Option<JoinHandle<()>>,
    /// What was last seen, to notice changes.
    seen: State,
    /// When the state last changed, until it is written.
    changed: Option<Instant>,
}

impl Persister {
    /// Loads the saved state into `app` and starts remembering it.
    pub(crate) fn open(dir: &DataDir, app: &mut App) -> Self {
        let paths = Paths::of(dir);
        let saved = fs::read_to_string(&paths.file).ok();
        let mut restored = Restored::parse(saved.as_deref().unwrap_or("{}"));
        if restored.widths.is_none() {
            // Nothing saved here yet: the widths older versions kept.
            let old = prefs::load(&paths.legacy);
            restored.widths = Some((old.left_width, old.right_width));
        }
        restored.apply(app);
        let (jobs, queue) = mpsc::channel::<Job>();
        let file = paths.file.clone();
        let writer = std::thread::Builder::new()
            .name("ui-state".to_owned())
            .spawn(move || {
                for job in queue {
                    write_job(&file, job);
                }
            })
            .ok();
        Self {
            jobs: writer.is_some().then_some(jobs),
            writer,
            seen: State::of(app),
            changed: None,
        }
    }

    /// Notices a change in `app` and writes it once it has been quiet for [`DEBOUNCE`].
    pub(crate) fn tick(&mut self, app: &App, now: Instant) {
        if app.view.sample {
            return;
        }
        let state = State::of(app);
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
        let Ok(json) = serde_json::to_string_pretty(&State::of(app)) else {
            return false;
        };
        let sent = jobs.send(Job { json, done }).is_ok();
        if sent {
            self.seen = State::of(app);
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

/// Writes one snapshot to `ui.json`, then tells whoever waits for it.
fn write_job(file: &Path, job: Job) {
    let _ = write_atomic(file, job.json.as_bytes());
    if let Some(done) = job.done {
        let _ = done.send(());
    }
}

/// Writes `bytes` to `path` all at once or not at all: a temporary file at mode 0600, flushed to
/// disk, renamed into place, and the directory flushed too.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
