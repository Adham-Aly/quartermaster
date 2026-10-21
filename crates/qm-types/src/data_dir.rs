//! The data directory.
//!
//! All state lives in one directory per Mac: `~/.quartermaster`, or `QUARTERMASTER_HOME` when it is
//! set (every test and development run sets it to a throwaway directory). This module only names
//! the paths; creating them with safe permissions at first run is the binary's job.
//!
//! ```text
//! config.toml   state.db   auth.json   run.lock   server.sock   logs/   workspace/
//! seat/{home,session}/                      the Quartermaster
//! secondmates/SM-02/{home,session}/
//! tasks/T-0042/{worktree/,brief.md,skills/,session/,jobs/}
//! ```
//!
//! Temporary files live elsewhere, in the scratch root: `/tmp/quartermaster/<uid>/<hash>/`, where
//! `<hash>` is the first 12 hex digits of the SHA-256 of the data directory's path. It holds
//! `attachments/` (pasted images), `drafts/` (images in unsent drafts) and `evidence/<task>/`.
//! Nothing in it outlives the machine's own `/tmp` cleaner by design; see [`DataDir::ensure_scratch`].

use crate::env::QUARTERMASTER_HOME;
use crate::ids::{SecondmateId, TaskId};
use sha2::{Digest as _, Sha256};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
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
        Self::resolve_from(over.as_deref(), home.as_deref(), &cwd)
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

    /// This data directory's scratch root: attachments, drafts and task evidence.
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// Creates the scratch root and checks it before anything is written under it: the root and
    /// the folder above it (the user's) must pass [`private_folder`]. `/tmp/quartermaster` itself is
    /// made sticky and world-writable if this creates it.
    pub fn ensure_scratch(&self) -> io::Result<()> {
        let base = Path::new(SCRATCH_BASE);
        if self.scratch.starts_with(base) {
            if !base.exists() {
                fs::DirBuilder::new().mode(0o1777).create(base)?;
                // `create` applies the umask, which would strip the world-write bit.
                fs::set_permissions(base, fs::Permissions::from_mode(0o1777))?;
            }
            if let Some(user) = self.scratch.parent() {
                private_folder(user)?;
            }
        }
        private_folder(&self.scratch)
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

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The personal workspace's default folder (`[workspace] folder` changes it). Crewmates work
    /// in it directly, with no git; an older one may still be a git repository.
    pub fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
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
        self.scratch.join("attachments")
    }

    /// The images in unsent drafts: `drafts/<token>.<ext>`, written and removed by the UI host,
    /// and left alone by the cleanup in `qm_core::attachments`.
    pub fn draft_images(&self) -> PathBuf {
        self.scratch.join(DRAFT_IMAGES)
    }

    /// The UI's remembered state: layout, popups, drafts (`qm-tui` writes it).
    pub fn ui_state(&self) -> PathBuf {
        self.root.join("ui.json")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    /// A task's folders and files. Its evidence lives in the scratch root, not the data directory.
    pub fn task(&self, id: TaskId) -> TaskPaths {
        TaskPaths::at(
            self.tasks_dir().join(id.to_string()),
            self.scratch.join("evidence").join(id.to_string()),
        )
    }
}

/// The folder of draft images, inside the scratch root.
pub const DRAFT_IMAGES: &str = "drafts";

/// Makes `path` (and any folder above it) if absent, then checks that it is a real folder this
/// user owns, with mode 0700. A symlink, another owner or a wider mode is refused with a plain
/// error, so nothing is written under a folder someone else could read or swap.
pub fn private_folder(path: &Path) -> io::Result<()> {
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
}

impl AgentPaths {
    fn at(root: PathBuf) -> Self {
        Self {
            home: root.join("home"),
            session: root.join("session"),
            root,
        }
    }
}

/// A task's folders and files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskPaths {
    pub root: PathBuf,
    /// The git worktree on branch `qm/task-0042`.
    pub worktree: PathBuf,
    /// The brief exactly as sent.
    pub brief: PathBuf,
    /// The skills this crewmate gets, gathered by the app.
    pub skills: PathBuf,
    /// The engine's own state for this crewmate.
    pub session: PathBuf,
    /// Screenshots, recordings, logs, reproduction output; never committed (`QM_EVIDENCE_DIR`).
    /// In the scratch root, so it is temporary.
    pub evidence: PathBuf,
    /// Background shell job output.
    pub jobs: PathBuf,
}

impl TaskPaths {
    fn at(root: PathBuf, evidence: PathBuf) -> Self {
        Self {
            worktree: root.join("worktree"),
            brief: root.join("brief.md"),
            skills: root.join("skills"),
            session: root.join("session"),
            evidence,
            jobs: root.join("jobs"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scratch_root_is_per_user_and_per_data_directory() {
        let one = DataDir::new("/data/one");
        let two = DataDir::new("/data/two");
        assert_ne!(one.scratch(), two.scratch());
        assert_eq!(
            one.scratch().parent().and_then(Path::parent),
            Some(Path::new(SCRATCH_BASE))
        );
        let hash = one
            .scratch()
            .file_name()
            .and_then(OsStr::to_str)
            .expect("a hash");
        assert_eq!(hash.len(), 12);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn attachments_drafts_and_task_evidence_live_under_the_scratch_root() {
        let dir = DataDir::with_scratch("/data", "/scratch");
        let task = dir.task(TaskId::new(42).expect("a task number"));
        assert_eq!(dir.attachments(), Path::new("/scratch/attachments"));
        assert_eq!(dir.draft_images(), Path::new("/scratch/drafts"));
        assert_eq!(task.evidence, Path::new("/scratch/evidence/T-0042"));
        assert_eq!(task.jobs, Path::new("/data/tasks/T-0042/jobs"));
    }

    #[test]
    fn a_scratch_root_that_is_a_symlink_is_refused() {
        let base = std::env::temp_dir().join(format!("qm-scratch-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("a test folder");
        let target = base.join("elsewhere");
        fs::create_dir_all(&target).expect("a target");
        let scratch = base.join("scratch");
        std::os::unix::fs::symlink(&target, &scratch).expect("a symlink");
        let dir = DataDir::with_scratch(base.join("data"), &scratch);
        assert!(dir.ensure_scratch().is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_private_scratch_root_is_created_and_accepted() {
        let base = std::env::temp_dir().join(format!("qm-scratch-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let scratch = base.join("1000").join("abcdef012345");
        let dir = DataDir::with_scratch(base.join("data"), &scratch);
        dir.ensure_scratch().expect("a private scratch root");
        let mode = fs::metadata(&scratch)
            .expect("the folder")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        let _ = fs::remove_dir_all(&base);
    }
}
