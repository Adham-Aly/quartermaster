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
//! tasks/T-0042/{worktree/,brief.md,skills/,session/,evidence/,jobs/,answer.md}
//! ```

use crate::env::QUARTERMASTER_HOME;
use crate::ids::{SecondmateId, TaskId};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The data directory's root and every path in its layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

/// Why the data directory couldn't be located.
#[derive(Debug, thiserror::Error)]
pub enum DataDirError {
    #[error("neither QUARTERMASTER_HOME nor HOME is set, so the data directory can't be located")]
    NoHome,
    #[error("the current directory can't be read to resolve a relative QUARTERMASTER_HOME: {0}")]
    CurrentDir(#[source] std::io::Error),
}

impl DataDir {
    /// A data directory at exactly `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
        self.root.join("attachments")
    }

    /// The images in unsent drafts, kept for `ui.json`: `attachments/drafts/<token>.<ext>`, written
    /// and removed by the UI host, and left alone by the cleanup in `qm_core::attachments`.
    pub fn draft_images(&self) -> PathBuf {
        self.attachments().join(DRAFT_IMAGES)
    }

    /// The UI's remembered state: layout, popups, drafts (`qm-tui` writes it).
    pub fn ui_state(&self) -> PathBuf {
        self.root.join("ui.json")
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    /// A task's folders and files.
    pub fn task(&self, id: TaskId) -> TaskPaths {
        TaskPaths::at(self.tasks_dir().join(id.to_string()))
    }
}

/// The folder of draft images inside `attachments/`.
pub const DRAFT_IMAGES: &str = "drafts";

/// An orchestrator's folders: the Quartermaster's or a secondmate's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPaths {
    pub root: PathBuf,
    /// Its working folder, kept empty.
    pub home: PathBuf,
    /// Our own JSONL, or the mirrored Claude transcript.
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
    /// This crewmate's transcript.
    pub session: PathBuf,
    /// Screenshots, recordings, logs, reproduction output; never committed (`QM_EVIDENCE_DIR`).
    pub evidence: PathBuf,
    /// Background shell job output.
    pub jobs: PathBuf,
    /// The answer of an answer-only task.
    pub answer: PathBuf,
}

impl TaskPaths {
    fn at(root: PathBuf) -> Self {
        Self {
            worktree: root.join("worktree"),
            brief: root.join("brief.md"),
            skills: root.join("skills"),
            session: root.join("session"),
            evidence: root.join("evidence"),
            jobs: root.join("jobs"),
            answer: root.join("answer.md"),
            root,
        }
    }
}
