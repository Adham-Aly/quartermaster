//! The data directory.
//!
//! All state lives in one directory per Mac: `~/.quartermaster`, or `QUARTERMASTER_HOME` when it is
//! set (every test and development run sets it to a throwaway directory). This module only names
//! the paths; creating them with safe permissions at first run is the binary's job.
//!
//! ```text
//! config.toml   state.db   auth.json   run.lock   server.sock   logs/
//! seat/{home,session,transcript}/           the Quartermaster
//! secondmates/S-02/{home,session,transcript}/
//! tasks/C-0042/{worktree/,brief.md,skills/,session/,transcript/,jobs/}   a crewmate's own folder
//! artifacts/T-0017/C-0042/{plan,evidence,report}/   its plan, evidence and report, under its task
//! attachments/message-<id>/                 images pasted into a chat
//! drafts/                                   images in unsent drafts
//! ```
//!
//! `transcript/` holds the provider engine's own conversation files (`<session>.jsonl`); Claude
//! keeps its own transcripts, which the app never reads. What may be deleted, and when, is decided
//! in one place, `qm_core::retention`.
//!
//! A crewmate's own folder is named after it and never moves, whatever task, project or folder
//! it moves to: its worktree is always `worktree/` in it. One started before crewmates had names
//! keeps its `tasks/T-0042/`.
//! Its artifacts and scratchpad sit under its task and move when its task changes.
//!
//! Only scratchpads live elsewhere, in the scratch root: `/tmp/quartermaster/<uid>/<hash>/`, where
//! `<hash>` is the first 12 hex digits of the SHA-256 of the data directory's path. Older versions
//! also kept attachments, drafts, plans and evidence there; startup moves them into the data
//! directory (the `legacy_*` paths). See [`DataDir::ensure_scratch`].

use crate::env::QUARTERMASTER_HOME;
use crate::ids::{CrewmateId, SecondmateId, TaskLabelId};
use sha2::{Digest as _, Sha256};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

/// Where temporary files go: the literal `/tmp`, not `$TMPDIR`.
pub const SCRATCH_BASE: &str = "/tmp/quartermaster";

/// The data directory's root, every path in its layout, and its scratch root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
    scratch: PathBuf,
}

/// Why the data directory couldn't be located.
#[derive(Debug, thiserror::Error)]
pub enum DataDirError {
    #[error("neither QUARTERMASTER_HOME nor HOME is set, so the data directory can't be located")]
    NoHome,
    #[error("the current directory can't be read to resolve a relative QUARTERMASTER_HOME: {0}")]
    CurrentDir(#[source] io::Error),
    #[error("unsafe development scratch override: {0}")]
    ScratchOverride(String),
}

impl DataDir {
    /// A data directory at exactly `root`, with its scratch root under [`SCRATCH_BASE`] for this
    /// user.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let uid = rustix::process::getuid().as_raw();
        let scratch = scratch_for(Path::new(SCRATCH_BASE), uid, &root);
        Self { root, scratch }
    }

    /// A data directory at `root` whose scratch root is `scratch` (tests, and anything that must
    /// not touch the real `/tmp`).
    pub fn with_scratch(root: impl Into<PathBuf>, scratch: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            scratch: scratch.into(),
        }
    }

    /// The data directory for this process: `QUARTERMASTER_HOME` if set and non-empty (relative
    /// values are taken from the current directory), otherwise `$HOME/.quartermaster`.
    pub fn resolve() -> Result<Self, DataDirError> {
        let over = std::env::var_os(QUARTERMASTER_HOME);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let needs_cwd = over
            .as_deref()
            .is_some_and(|o| !o.is_empty() && Path::new(o).is_relative());
        let cwd = if needs_cwd {
            std::env::current_dir().map_err(DataDirError::CurrentDir)?
        } else {
            PathBuf::from("/")
        };
        let mut data = Self::resolve_from(over.as_deref(), home.as_deref(), &cwd)?;
        if let Some(scratch) = std::env::var_os("QM_DEV_SCRATCH") {
            let scratch = development_scratch(
                &data.root,
                home.as_deref(),
                over.as_deref(),
                std::env::var_os("QM_DEV").as_deref(),
                cfg!(debug_assertions),
                scratch,
            )?;
            data.scratch = scratch;
        }
        Ok(data)
    }

    /// [`DataDir::resolve`] without touching the environment: `home_override` is the value of
    /// `QUARTERMASTER_HOME`, `home_dir` the user's home folder, `cwd` the current directory.
    pub fn resolve_from(
        home_override: Option<&OsStr>,
        home_dir: Option<&Path>,
        cwd: &Path,
    ) -> Result<Self, DataDirError> {
        match home_override.filter(|o| !o.is_empty()) {
            Some(over) => Ok(Self::new(cwd.join(over))),
            None => home_dir
                .map(|h| Self::new(h.join(".quartermaster")))
                .ok_or(DataDirError::NoHome),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This data directory's scratch root: crewmates' scratchpads (and, from older versions, files
    /// startup moves into the data directory).
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// Creates the scratch root and checks it before anything is written under it: the root and
    /// the folder above it (the user's) must pass [`private_folder`]. `/tmp/quartermaster` itself is
    /// made sticky and world-writable if this creates it.
    pub fn ensure_scratch(&self) -> io::Result<()> {
        let base = Path::new(SCRATCH_BASE);
        if self.scratch.starts_with(base) {
            shared_base(base)?;
            if let Some(user) = self.scratch.parent() {
                private_folder(user)?;
            }
        }
        private_folder(&self.scratch)
    }

    /// Validate the scratch hierarchy and every directory down to `path` before I/O.
    /// Rejects links, traversal and non-private directories; creates missing directories privately.
    pub fn ensure_scratch_dir(&self, path: &Path) -> io::Result<()> {
        self.ensure_scratch()?;
        let relative = path.strip_prefix(&self.scratch).map_err(io::Error::other)?;
        let mut current = self.scratch.clone();
        for component in relative.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(io::Error::other("invalid scratch path"));
            }
            current.push(component);
            private_folder(&current)?;
        }
        Ok(())
    }

    /// Your settings (no secrets).
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The SQLite database, the source of truth for everything the app knows.
    pub fn database(&self) -> PathBuf {
        self.root.join("state.db")
    }

    /// Credentials for non-Claude accounts, mode 0600.
    pub fn auth_file(&self) -> PathBuf {
        self.root.join("auth.json")
    }

    /// Held while the app runs; a second instance refuses to start.
    pub fn run_lock(&self) -> PathBuf {
        self.root.join("run.lock")
    }

    /// The server's socket, bound only while the server holds `run.lock`.
    pub fn server_socket(&self) -> PathBuf {
        self.root.join("server.sock")
    }

    /// The notifier's socket: the menu bar helper connects to it while the server runs.
    pub fn notifier_socket(&self) -> PathBuf {
        self.root.join("notifier.sock")
    }

    /// The menu bar helper's app bundle (`qm-notify`), installed and signed by the server.
    pub fn notifier_app(&self) -> PathBuf {
        self.root.join("Quartermaster.app")
    }

    /// The plan pages' endpoint port, kept so a page rendered before a restart still sends
    /// (`{"port": 41041}`, mode 0600).
    pub fn plan_endpoint(&self) -> PathBuf {
        self.root.join("plan-endpoint.json")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The Quartermaster's folders.
    pub fn seat(&self) -> AgentPaths {
        AgentPaths::at(self.root.join("seat"))
    }

    pub fn secondmates_dir(&self) -> PathBuf {
        self.root.join("secondmates")
    }

    /// A secondmate's folders.
    pub fn secondmate(&self, id: SecondmateId) -> AgentPaths {
        AgentPaths::at(self.secondmates_dir().join(id.to_string()))
    }

    /// Images you pasted into a chat: `attachments/message-<id>/image-<n>.<ext>`, one folder per
    /// message, removed by the cleanup policy in `qm_core::attachments`.
    pub fn attachments(&self) -> PathBuf {
        self.root.join(ATTACHMENTS)
    }

    /// The images in unsent drafts: `drafts/<token>.<ext>`, written and removed by the UI host,
    /// and left alone by the cleanup in `qm_core::attachments`.
    pub fn draft_images(&self) -> PathBuf {
        self.root.join(DRAFT_IMAGES)
    }

    /// Where older versions kept pasted images, in the scratch root.
    pub fn legacy_attachments(&self) -> PathBuf {
        self.scratch.join(ATTACHMENTS)
    }

    /// Where older versions kept draft images, in the scratch root.
    pub fn legacy_draft_images(&self) -> PathBuf {
        self.scratch.join(DRAFT_IMAGES)
    }

    /// Where older versions kept a crewmate's plan, in the scratch root.
    pub fn legacy_plan(&self, id: CrewmateId) -> PathBuf {
        self.scratch.join(PLANS).join(id.legacy_name())
    }

    /// Where older versions kept a crewmate's evidence, in the scratch root.
    pub fn legacy_evidence(&self, id: CrewmateId) -> PathBuf {
        self.scratch.join(EVIDENCE).join(id.legacy_name())
    }

    /// Where older versions kept a crewmate's plan, evidence and report: `artifacts/T-0042/`,
    /// named after the crewmate's number, with no task level.
    pub fn legacy_artifacts(&self, id: CrewmateId) -> PathBuf {
        self.artifacts_dir().join(id.legacy_name())
    }

    /// Where older versions kept a crewmate's scratchpad: `scratchpad/T-0042/`.
    pub fn legacy_scratchpad(&self, id: CrewmateId) -> PathBuf {
        self.scratchpads_dir().join(id.legacy_name())
    }

    /// The folder of every task's scratchpads, in the scratch root.
    pub fn scratchpads_dir(&self) -> PathBuf {
        self.scratch.join(SCRATCHPAD)
    }

    /// `artifacts/T-0017/`: the artifacts of every crewmate of one task.
    pub fn task_artifacts(&self, label: TaskLabelId) -> PathBuf {
        self.artifacts_dir().join(label.to_string())
    }

    /// The app's own folders, the data directory and the scratch root: an agent writes inside them
    /// only through a write root strictly inside one (its worktree, evidence folder, scratchpad).
    pub fn protected_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone(), self.scratch.clone()]
    }

    /// `artifacts/`: each crewmate's plan, evidence and report, under its task:
    /// `artifacts/T-0017/C-0042/{plan,evidence,report}`.
    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join(ARTIFACTS)
    }

    /// The UI's remembered state: layout, popups, drafts (`qm-tui` writes it).
    pub fn ui_state(&self) -> PathBuf {
        self.root.join("ui.json")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join(TASKS)
    }

    /// A crewmate's folders and files, with its artifacts and scratchpad under `folders`, the task
    /// they are filed under now (`tasks.folder_label_id`). Its plan, evidence and report live in
    /// `artifacts/`, apart from its own folder, because they can outlive it (a build reads its
    /// plan crewmate's plan); only its scratchpad is in the scratch root.
    pub fn task(&self, id: CrewmateId, folders: TaskLabelId) -> TaskPaths {
        TaskPaths::at(
            self.crewmate_root(id),
            self.task_artifacts(folders).join(id.to_string()),
            self.scratchpads_dir()
                .join(folders.to_string())
                .join(id.to_string()),
        )
    }

    /// Every crewmate's artifacts folder that is a real folder (a link a relabel left is skipped),
    /// under its task: `(T-0017, C-0042, artifacts/T-0017/C-0042)`.
    pub fn filed_artifacts(&self) -> Vec<(TaskLabelId, CrewmateId, PathBuf)> {
        let mut out = Vec::new();
        for (label, dir) in named_dirs::<TaskLabelId>(&self.artifacts_dir()) {
            for (crewmate, path) in named_dirs::<CrewmateId>(&dir) {
                if path.file_name().and_then(OsStr::to_str) == Some(&crewmate.to_string()) {
                    out.push((label, crewmate, path));
                }
            }
        }
        out
    }

    /// A crewmate's worktree: `tasks/C-0042/worktree/`. It doesn't depend on the crewmate's task.
    pub fn worktree(&self, id: CrewmateId) -> PathBuf {
        self.crewmate_root(id).join(WORKTREE)
    }

    /// A crewmate's own folder: `tasks/C-0042/`, or `tasks/T-0042/` when an older version made
    /// it. It never moves, whatever task, project or folder the crewmate moves to.
    pub fn crewmate_root(&self, id: CrewmateId) -> PathBuf {
        let current = self.tasks_dir().join(id.to_string());
        let legacy = self.tasks_dir().join(id.legacy_name());
        if fs::symlink_metadata(&current).is_err() && fs::symlink_metadata(&legacy).is_ok() {
            legacy
        } else {
            current
        }
    }
}

/// The real folders in `dir` (no links) whose names parse as `I`, with their paths.
fn named_dirs<I: std::str::FromStr>(dir: &Path) -> Vec<(I, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path())))
        .collect()
}

fn development_scratch(
    root: &Path,
    home: Option<&Path>,
    explicit: Option<&OsStr>,
    dev: Option<&OsStr>,
    debug: bool,
    scratch: impl Into<PathBuf>,
) -> Result<PathBuf, DataDirError> {
    let is_real_home = home.is_some_and(|home| {
        let real = home.join(".quartermaster");
        root == real
            || root
                .canonicalize()
                .ok()
                .zip(real.canonicalize().ok())
                .is_some_and(|(a, b)| a == b)
    });
    if !debug
        || dev != Some(OsStr::new("1"))
        || explicit.is_none_or(OsStr::is_empty)
        || is_real_home
        || root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(DataDirError::ScratchOverride(
            "requires a debug binary, QM_DEV=1 and a throwaway QUARTERMASTER_HOME".into(),
        ));
    }
    let scratch = scratch.into();
    if !scratch.is_absolute()
        || scratch.starts_with(SCRATCH_BASE)
        || scratch.starts_with("/private/tmp/quartermaster")
        || scratch
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(DataDirError::ScratchOverride(
            "use an absolute unique path outside /tmp/quartermaster".into(),
        ));
    }
    Ok(scratch)
}

/// The folder of every crewmate's own folder (`tasks/C-n/`), in the data directory.
pub const TASKS: &str = "tasks";

/// A crewmate's brief exactly as sent, inside its `tasks/C-n/`.
pub const BRIEF: &str = "brief.md";

/// A crewmate's worktree, inside its `tasks/C-n/`.
pub const WORKTREE: &str = "worktree";

/// The folder of every crewmate's plan, evidence and report, by task, in the data directory.
pub const ARTIFACTS: &str = "artifacts";

/// A crewmate's plan folder, inside its `artifacts/T-n/C-n/`; also older versions' folder of plans in
/// the scratch root.
pub const PLANS: &str = "plans";

/// A task's plan folder name inside `artifacts/T-n/`.
pub const PLAN: &str = "plan";

/// A task's evidence folder name inside `artifacts/T-n/` (and in older scratch roots).
pub const EVIDENCE: &str = "evidence";

/// The folder of pasted images, in the data directory.
pub const ATTACHMENTS: &str = "attachments";

/// The folder of draft images, in the data directory.
pub const DRAFT_IMAGES: &str = "drafts";

/// An agent's or task's folder of provider-engine transcripts.
pub const TRANSCRIPT: &str = "transcript";

/// The folder of scratchpads, in the scratch root.
pub const SCRATCHPAD: &str = "scratchpad";

/// Makes `path` (and any folder above it) if absent, then checks that it is a real folder this
/// user owns, with mode 0700. A symlink, another owner or a wider mode is refused with a plain
/// error, so nothing is written under a folder someone else could read or swap.
pub fn private_folder(path: &Path) -> io::Result<()> {
    // macOS has system-owned /tmp and /var aliases; no other symlink ancestor is allowed.
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let meta = match fs::symlink_metadata(ancestor) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() {
            if (ancestor == Path::new("/tmp") || ancestor == Path::new("/var")) && meta.uid() == 0 {
                continue;
            }
            return Err(io::Error::other("a folder ancestor is a symlink"));
        }
        let trusted_owner = meta.uid() == 0 || meta.uid() == rustix::process::getuid().as_raw();
        if !trusted_owner || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0) {
            return Err(io::Error::other(
                "a folder ancestor can be replaced by another user",
            ));
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{} is not a plain folder, so nothing is written under it",
            path.display()
        )));
    }
    if meta.uid() != rustix::process::getuid().as_raw() || meta.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(format!(
            "{} isn't private to this user (owner or mode), so nothing is written under it",
            path.display()
        )));
    }
    Ok(())
}

/// Validates (creating privately) an attachments, drafts or evidence folder and every folder above
/// it up to its trust root: the data directory, or the shared scratch hierarchy for a path in it.
pub fn ensure_scratch_folder(path: &Path) -> io::Result<()> {
    if let Ok(relative) = path.strip_prefix(SCRATCH_BASE) {
        let parts: Vec<_> = relative.components().collect();
        if parts.len() < 2 {
            return Err(io::Error::other("incomplete scratch hierarchy"));
        }
        let root = Path::new(SCRATCH_BASE).join(parts[0]).join(parts[1]);
        DataDir::with_scratch("/unused", root).ensure_scratch_dir(path)
    } else {
        // These named children carry their own parent scratch trust boundary, including in
        // isolated developer runs. Revalidate it even after successful startup.
        let mut ancestor = path;
        while let Some(parent) = ancestor.parent() {
            if matches!(
                ancestor.file_name().and_then(OsStr::to_str),
                Some("attachments" | "drafts" | "evidence")
            ) {
                return DataDir::with_scratch("/unused", parent).ensure_scratch_dir(path);
            }
            ancestor = parent;
        }
        private_folder(path)
    }
}

/// Moves a legacy directory without overwriting any existing destination, leaving a symlink at the
/// old path so absolute paths an agent was given keep working. A cross-device move copies and
/// verifies every regular file before removing the source. A conflict or an unsupported entry
/// keeps the original directory intact.
pub fn migrate_directory(source: &Path, target: &Path) -> io::Result<()> {
    migrate_directory_with(source, target, |source, target| fs::rename(source, target))
}

fn migrate_directory_with(
    source: &Path,
    target: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let meta = match fs::symlink_metadata(source) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(io::Error::other("legacy source is not a directory"));
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    ensure_scratch_folder(parent)?;
    if fs::symlink_metadata(target).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "migration conflict; original preserved",
        ));
    }
    match rename(source, target) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
            copy_verified(source, target)?;
            fs::remove_dir_all(source)?;
        }
        Err(e) => return Err(e),
    }
    std::os::unix::fs::symlink(target, source)
}

fn copy_verified(source: &Path, target: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(target)?;
    // Only this call's newly created directory belongs to us. In particular, a failed
    // create must never cause an existing destination (or a link) to be removed.
    if let Err(error) = copy_verified_contents(source, target) {
        fs::remove_dir_all(target)?;
        return Err(error);
    }
    Ok(())
}

fn copy_verified_contents(source: &Path, target: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let dest = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_verified(&entry.path(), &dest)?;
        } else if kind.is_file() {
            let bytes = fs::read(entry.path())?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&dest)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            if fs::read(&dest)? != bytes || fs::read(entry.path())? != bytes {
                return Err(io::Error::other("migration copy verification failed"));
            }
        } else {
            return Err(io::Error::other(
                "unsupported legacy entry; source preserved",
            ));
        }
    }
    Ok(())
}

/// The shared folder above every user's scratch: created sticky and world-writable when missing,
/// and checked on every use. It must be a plain folder (not a link), sticky, and owned by root or by
/// this user: the sticky bit keeps other users from renaming or removing what is inside it, and
/// the owner check stops a folder another user made from receiving our files.
fn shared_base(base: &Path) -> io::Result<()> {
    if fs::symlink_metadata(base).is_err() {
        match fs::DirBuilder::new().mode(0o1777).create(base) {
            Ok(()) => {
                // `create` applies the umask, which would strip the world-write bit.
                fs::set_permissions(base, fs::Permissions::from_mode(0o1777))?;
            }
            // Another process made it first: check that one like any other.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    check_shared_base(base)
}

fn check_shared_base(base: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(base)?;
    let me = rustix::process::getuid().as_raw();
    let trusted_owner = meta.uid() == 0 || meta.uid() == me;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || !trusted_owner
        || meta.mode() & 0o1000 == 0
    {
        return Err(io::Error::other(format!(
            "{} isn't a plain sticky folder owned by you or root, so nothing is written under it",
            base.display()
        )));
    }
    Ok(())
}

/// The scratch root for a data directory: `<base>/<uid>/<first 12 hex digits of SHA-256(root)>`.
fn scratch_for(base: &Path, uid: u32, root: &Path) -> PathBuf {
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let mut hash = String::with_capacity(12);
    for byte in &digest[..6] {
        // Writing to a `String` can't fail.
        let _ = write!(hash, "{byte:02x}");
    }
    base.join(uid.to_string()).join(hash)
}

/// An orchestrator's folders: the Quartermaster's or a secondmate's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPaths {
    pub root: PathBuf,
    /// Its working folder, kept empty.
    pub home: PathBuf,
    /// The engine's own state for this agent.
    pub session: PathBuf,
    /// The provider engine's transcripts of its conversation (`<session>.jsonl`).
    pub transcript: PathBuf,
}

impl AgentPaths {
    fn at(root: PathBuf) -> Self {
        Self {
            home: root.join("home"),
            session: root.join("session"),
            transcript: root.join(TRANSCRIPT),
            root,
        }
    }
}

/// A crewmate's folders and files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskPaths {
    pub root: PathBuf,
    /// The git worktree, on the task's branch (`fix/upload-retry-on-slow-tmp`).
    pub worktree: PathBuf,
    /// The brief exactly as sent.
    pub brief: PathBuf,
    /// The skills this crewmate gets, gathered by the app.
    pub skills: PathBuf,
    /// The engine's own state for this crewmate.
    pub session: PathBuf,
    /// The provider engine's transcripts of this crewmate's conversation.
    pub transcript: PathBuf,
    /// `artifacts/T-0017/C-0042/`: this crewmate's plan, evidence and report, which can outlive it.
    pub artifacts: PathBuf,
    /// Screenshots, recordings, logs, reproduction output; never committed (`QM_EVIDENCE_DIR`).
    pub evidence: PathBuf,
    /// Background shell job output.
    pub jobs: PathBuf,
    /// The plan this task writes or builds from: `plan.md`, `plan.html`, `assets/`, the app's
    /// `state.json` (`QM_PLAN_DIR`).
    pub plan: PathBuf,
    /// The provider-engine crewmate's private scratchpad (and its helpers'), for temporary files.
    /// In the scratch root; made only for provider sessions and removed when the task closes.
    pub scratchpad: PathBuf,
}

impl TaskPaths {
    fn at(root: PathBuf, artifacts: PathBuf, scratchpad: PathBuf) -> Self {
        Self {
            plan: artifacts.join(PLAN),
            evidence: artifacts.join(EVIDENCE),
            artifacts,
            worktree: root.join(WORKTREE),
            brief: root.join(BRIEF),
            skills: root.join("skills"),
            session: root.join("session"),
            transcript: root.join(TRANSCRIPT),
            jobs: root.join("jobs"),
            scratchpad,
            root,
        }
    }
}

/// Removes the file or folder at `path`, with everything in it. Nothing there is not an error.
/// A symlink is removed itself, never followed.
pub fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
