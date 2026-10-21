//! The project registry.
//!
//! A project is a local git repository with settings. It joins the list only with your
//! confirmation: `propose_project` stores a held action whose payload is exactly what will happen
//! (link this folder, clone this URL into this folder, create this folder), with a suggested
//! location filled in, and the [`AddProjectPerformer`] does exactly that when you approve. Nothing
//! is scanned automatically; `find_project_folder` looks only when asked, in the search roots from
//! settings ([`find_folders`]).
//!
//! - **Linking never moves or copies** anything: the folder is registered where it is.
//! - **Removing never deletes files**: the row is marked removed and the folder left alone.
//! - **`configure_project` can't change autonomy or the verification level** (its arguments have no
//!   field for either), and checks what it can change against the facts: a direct-PR project needs
//!   a GitHub remote, a branch-only one a remote, and the personal workspace stays branch-only.
//! - **The personal workspace** ([`ProjectService::ensure_workspace`]) is a plain folder, by default
//!   `<data dir>/workspace` or the one `[workspace] folder` names. Its delivery is `folder`: no
//!   git runs in it, its tasks have no branches, and changing the folder refuses while personal
//!   tasks are open ([`move_workspace`]). Only the personal workspace can be a plain folder.
//!
//! [`ProjectsPart`] fills the Bridge's projects pane.

use crate::api::{ApiPart, Conversation, Handled};
use crate::held::{HeldCard, HeldError, HeldOutcome, HeldPerformer, HeldService, LoadedHeld};
use crate::setup_check::SetupFacts;
use crate::turn_io::ChatLog;
use qm_git::{Git, Repo};
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::held::{AddProjectPayload, HeldPayload, NewHeld};
use qm_store::messages::{self, NewMessage};
use qm_store::projects::{self, NewProject, ProjectRow, SettingsChange};
use qm_tools::defs::{Ack, ConfigureProjectArgs, Held, ProposeProjectArgs};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::BoxFuture;
use qm_types::api::{CardAction, CardButton, CardLine, HeldAnswer, Snapshot, Styled, Tone};
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::data_dir::DataDir;
use qm_types::domain::{
    Autonomy, DeliveryMode, HeldKind, MergeMethod, MessageKind, MessageSource, ProjectAction,
    ProjectOrigin,
};
use qm_types::error::Refusal;
use qm_types::ids::{HeldActionId, ProjectId};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::sync::watch;

/// How deep `find_project_folder` looks under each search root.
pub const SEARCH_DEPTH: usize = 3;
/// The most folders `find_project_folder` returns.
pub const MAX_FOUND: usize = 20;
/// The personal workspace's name in the project list.
pub const WORKSPACE_NAME: &str = "personal";
/// The suggested location when no project has been linked on this Mac yet.
pub const DEFAULT_LOCATION: &str = "~/Developer";
/// The merge method a new project starts with; you change it with `configure_project`.
pub const DEFAULT_MERGE_METHOD: MergeMethod = MergeMethod::Squash;
/// The first commit of a created project or the workspace, so worktrees have a base.
const FIRST_COMMIT: &str = "chore: start the repository";
/// Who makes that commit when your git has no identity configured.
const APP_IDENTITY: (&str, &str) = ("Quartermaster", "quartermaster@localhost");

/// Event kinds the registry writes.
pub mod event_kind {
    pub const CONFIGURED: &str = "project.configured";
    pub const REMOVED: &str = "project.removed";
    pub const ADDED: &str = "project.added";
    /// You turned a project's autonomy on or off (never an agent).
    pub const AUTONOMY: &str = "project.autonomy";
}

/// Everything the registry needs.
pub struct ProjectDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub data_dir: DataDir,
    /// Settings: the search roots live here (per Mac).
    pub config: watch::Receiver<Config>,
    /// The setup check: whether `git` and `gh` are there.
    pub setup: watch::Receiver<SetupFacts>,
    /// The app's environment, for `git` and `gh` (its `HOME` is your home folder).
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for ProjectDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectDeps")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

/// The project registry.
#[derive(Clone)]
pub struct ProjectService {
    inner: Arc<ProjectDeps>,
}

impl fmt::Debug for ProjectService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectService").finish_non_exhaustive()
    }
}

fn refused(reason: impl Into<String>) -> ToolFailure {
    Refusal::new(reason).into()
}

fn failed(e: &StoreError) -> ToolFailure {
    ToolFailure::Failed(e.to_string())
}

/// Your home folder, from the environment.
fn home_of(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `~` and `~/…` expanded against `home`.
pub fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    let path = path.trim();
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// `path` with your home folder written `~`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The personal workspace's folder: the one `[workspace] folder` names, `~` expanded, else
/// `<data dir>/workspace`.
pub(crate) fn workspace_folder(
    config: &Config,
    data_dir: &DataDir,
    home: Option<&Path>,
) -> PathBuf {
    match config
        .workspace
        .folder
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        Some(folder) => expand(folder, home),
        None => data_dir.workspace(),
    }
}

/// Checks a folder chosen for the personal workspace before anything changes: an absolute path (or
/// one from `~`) to an existing folder that isn't read-only and isn't one of the app's task
/// folders. Returns it expanded.
pub(crate) fn check_workspace_folder(
    folder: &str,
    home: Option<&Path>,
    data_dir: &DataDir,
) -> Result<PathBuf, String> {
    let text = folder.trim();
    if text.is_empty() {
        return Err("the personal folder needs a path".to_owned());
    }
    if !(text.starts_with('/') || text == "~" || text.starts_with("~/")) {
        return Err(format!(
            "{text} isn't an absolute path: write it from / or from ~"
        ));
    }
    let path = expand(text, home);
    if !path.is_dir() {
        return Err(format!("{} isn't a folder", path.display()));
    }
    if path.metadata().is_ok_and(|m| m.permissions().readonly()) {
        return Err(format!(
            "{} is read-only, so crewmates can't work in it",
            path.display()
        ));
    }
    if path.starts_with(data_dir.tasks_dir()) {
        return Err(format!(
            "{} is one of the app's task folders, so it can't be the personal folder",
            path.display()
        ));
    }
    Ok(path)
}

/// Points the personal workspace at `folder` (already checked by [`check_workspace_folder`]).
/// Refuses while personal tasks are open, or when another project is registered there. No files
/// move. With no workspace registered yet, there's nothing to move: the next start registers it.
pub(crate) fn move_workspace(conn: &Connection, folder: &Path) -> Result<(), String> {
    let store = |e: StoreError| e.to_string();
    let Some(workspace) = projects::workspace(conn).map_err(store)? else {
        return Ok(());
    };
    if workspace.path == folder {
        return Ok(());
    }
    let open = projects::open_tasks(conn, workspace.id).map_err(store)?;
    if !open.is_empty() {
        let names: Vec<String> = open.iter().map(ToString::to_string).collect();
        return Err(format!(
            "personal tasks are still open ({}); close them first, then change the personal folder",
            names.join(", ")
        ));
    }
    if let Some(other) = projects::active_by_path(conn, folder).map_err(store)?
        && other.id != workspace.id
    {
        return Err(format!(
            "{} is already the project {}",
            folder.display(),
            other.name
        ));
    }
    projects::set_path(conn, workspace.id, folder).map_err(store)?;
    Ok(())
}

/// Resolves a search root case-insensitively: each component that doesn't exist as written is
/// matched against its parent's entries ignoring case.
fn resolve_root(root: &Path) -> Option<PathBuf> {
    if root.is_dir() {
        return Some(root.to_owned());
    }
    let mut at = PathBuf::new();
    for component in root.components() {
        let next = at.join(component);
        if next.exists() {
            at = next;
            continue;
        }
        let wanted = component.as_os_str().to_str()?.to_lowercase();
        let found = std::fs::read_dir(&at).ok()?.flatten().find(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.to_lowercase() == wanted)
        })?;
        at = found.path();
    }
    at.is_dir().then_some(at)
}

/// Folders named `name` (any case) containing a `.git`, up to [`SEARCH_DEPTH`] levels under each
/// root, best first: an exact-case match before others, shallower before deeper. Symbolic links to
/// folders are followed once each; nothing is read beyond the depth.
pub fn find_folders(roots: &[PathBuf], name: &str) -> Vec<PathBuf> {
    let wanted = name.to_lowercase();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut found: Vec<(bool, usize, PathBuf)> = Vec::new();
    for root in roots {
        let Some(root) = resolve_root(root) else {
            continue;
        };
        let Ok(canonical) = std::fs::canonicalize(&root) else {
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        let mut level = vec![root];
        for depth in 1..=SEARCH_DEPTH {
            let mut next = Vec::new();
            for dir in &level {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let file_name = entry.file_name();
                    let Some(file_name) = file_name.to_str() else {
                        continue;
                    };
                    if file_name == ".git" || !path.is_dir() {
                        continue;
                    }
                    let Ok(canonical) = std::fs::canonicalize(&path) else {
                        continue;
                    };
                    if !seen.insert(canonical) {
                        continue;
                    }
                    if file_name.to_lowercase() == wanted && path.join(".git").exists() {
                        found.push((file_name != name, depth, path.clone()));
                    }
                    next.push(path);
                }
            }
            level = next;
        }
    }
    found.sort();
    found
        .into_iter()
        .map(|(_, _, p)| p)
        .take(MAX_FOUND)
        .collect()
}

/// `owner/name` when `url` points at a GitHub repository (`https://github.com/o/n(.git)`,
/// `git@github.com:o/n(.git)`, `ssh://git@github.com/o/n`).
pub fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

/// `owner/name` shorthand for a GitHub repository.
fn is_shorthand(url: &str) -> bool {
    let mut parts = url.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(o), Some(n), None) if !o.is_empty() && !n.is_empty() && !url.contains(':')
            && !o.starts_with('.') && !o.starts_with('~')
    )
}

/// The delivery mode a new project starts with: direct pull requests with a GitHub remote,
/// branch-only with another remote, build in place without one.
pub fn default_delivery(github_remote: Option<&str>, has_remote: bool) -> DeliveryMode {
    if github_remote.is_some() {
        DeliveryMode::DirectPr
    } else if has_remote {
        DeliveryMode::BranchOnly
    } else {
        DeliveryMode::BuildInPlace
    }
}

fn delivery_words(mode: DeliveryMode) -> &'static str {
    match mode {
        DeliveryMode::DirectPr => "direct pull requests",
        DeliveryMode::BranchOnly => "branch only",
        DeliveryMode::BuildInPlace => "build in place",
        DeliveryMode::Folder => "a plain folder",
    }
}

/// Runs the app's own `git`: inherited `GIT_*` variables removed, nothing prompts.
fn git(
    program: &Path,
    env: &BTreeMap<String, String>,
    dir: &Path,
    args: &[&str],
) -> Result<String, String> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().filter(|(k, _)| !k.starts_with("GIT_")))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .current_dir(dir)
        .stdin(Stdio::null());
    let out = command
        .output()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                "git gave no reason"
            } else {
                &stderr
            }
        ))
    }
}

/// What registering a repository needs to know about it.
struct RepoFacts {
    root: PathBuf,
    default_branch: String,
    github_remote: Option<String>,
    has_remote: bool,
}

fn repo_facts(
    program: &Path,
    env: &BTreeMap<String, String>,
    path: &Path,
) -> Result<RepoFacts, String> {
    let repo = Repo::open(Git::with_program(program), path).map_err(|e| e.to_string())?;
    let default_branch = repo
        .detect_default_branch()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "main".to_owned());
    let remotes = repo.remotes().map_err(|e| e.to_string())?;
    let github_remote = if remotes.iter().any(|r| r == qm_git::ORIGIN) {
        git(
            program,
            env,
            repo.root(),
            &["remote", "get-url", qm_git::ORIGIN],
        )
        .ok()
        .and_then(|url| github_repo(&url))
    } else {
        None
    };
    Ok(RepoFacts {
        root: repo.root().to_owned(),
        default_branch,
        github_remote,
        has_remote: !remotes.is_empty(),
    })
}

/// `git init` in `dir` and an empty first commit, with your identity or, when git has none, the
/// app's.
fn init_repository(
    program: &Path,
    env: &BTreeMap<String, String>,
    dir: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    git(program, env, dir, &["init", "--quiet"])?;
    let commit = [
        "commit",
        "--quiet",
        "--allow-empty",
        "--no-verify",
        "-m",
        FIRST_COMMIT,
    ];
    if git(program, env, dir, &commit).is_ok() {
        return Ok(());
    }
    let (name, email) = APP_IDENTITY;
    let name = format!("user.name={name}");
    let email = format!("user.email={email}");
    let mut args = vec!["-c", &name, "-c", &email];
    args.extend(commit);
    git(program, env, dir, &args).map(|_| ())
}

/// What an approved proposal does on disk: nothing for a link (the folder stays exactly where it
/// is), a clone, or a new repository. Blocking.
struct Materialise {
    action: ProjectAction,
    target: PathBuf,
    url: Option<String>,
    /// `gh`, when it's installed and logged in: GitHub clones go through it.
    gh: Option<PathBuf>,
    git: PathBuf,
    env: BTreeMap<String, String>,
}

impl Materialise {
    fn run(self) -> Result<RepoFacts, String> {
        let Self {
            action,
            target,
            url,
            gh,
            git: program,
            env,
        } = self;
        match action {
            ProjectAction::Link => {}
            ProjectAction::Clone => {
                if target.exists() {
                    return Err(format!("{} already exists", target.display()));
                }
                let parent = target.parent().unwrap_or(Path::new("/"));
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("couldn't create {}: {e}", parent.display()))?;
                let url = url.unwrap_or_default();
                let dest = target.display().to_string();
                match (github_repo(&url), gh) {
                    (Some(repo), Some(gh)) => {
                        let out = std::process::Command::new(&gh)
                            .args(["repo", "clone", &repo, &dest])
                            .env_clear()
                            .envs(env.iter().filter(|(k, _)| !k.starts_with("GIT_")))
                            .env("GIT_TERMINAL_PROMPT", "0")
                            .env("GH_PROMPT_DISABLED", "1")
                            .current_dir(parent)
                            .stdin(Stdio::null())
                            .output()
                            .map_err(|e| format!("couldn't run gh: {e}"))?;
                        if !out.status.success() {
                            return Err(format!(
                                "`gh repo clone {repo}` failed: {}",
                                String::from_utf8_lossy(&out.stderr).trim()
                            ));
                        }
                    }
                    _ => {
                        git(&program, &env, parent, &["clone", "--quiet", &url, &dest])?;
                    }
                }
            }
            ProjectAction::Create => {
                if target.exists() {
                    return Err(format!("{} already exists", target.display()));
                }
                init_repository(&program, &env, &target)?;
            }
        }
        repo_facts(&program, &env, &target)
    }
}

impl ProjectService {
    pub fn new(deps: ProjectDeps) -> Self {
        Self {
            inner: Arc::new(deps),
        }
    }

    fn home(&self) -> Option<PathBuf> {
        home_of(&self.inner.env)
    }

    fn facts(&self) -> SetupFacts {
        self.inner.setup.borrow().clone()
    }

    fn git_program(&self) -> PathBuf {
        self.facts()
            .paths
            .git
            .unwrap_or_else(|| PathBuf::from("git"))
    }

    fn tilde(&self, path: &Path) -> String {
        tilde(path, self.home().as_deref())
    }

    /// `find_project_folder`: the matching folders, with `~` for your home folder.
    pub async fn find_folder(&self, name: &str) -> Result<Vec<String>, ToolFailure> {
        let home = self.home();
        let roots: Vec<PathBuf> = self
            .inner
            .config
            .borrow()
            .projects
            .search_roots
            .iter()
            .map(|r| expand(r, home.as_deref()))
            .collect();
        let name = name.to_owned();
        let found = tokio::task::spawn_blocking(move || find_folders(&roots, &name))
            .await
            .map_err(|e| ToolFailure::Failed(format!("the folder search stopped: {e}")))?;
        Ok(found.iter().map(|p| tilde(p, home.as_deref())).collect())
    }

    /// The suggested location: the parent folder of the most recently linked project on this
    /// Mac, or `~/Developer`.
    async fn suggested_location(&self) -> Result<PathBuf, StoreError> {
        let latest = self
            .inner
            .db
            .call(|conn| projects::latest_linked_path(conn))
            .await?;
        Ok(latest
            .and_then(|p| p.parent().map(Path::to_owned))
            .unwrap_or_else(|| expand(DEFAULT_LOCATION, self.home().as_deref())))
    }

    /// `propose_project`: checks the proposal against the facts, fills in the location, and stores
    /// it as a held action with a card in the conversation.
    pub async fn propose(
        &self,
        caller: &Caller,
        args: &ProposeProjectArgs,
    ) -> Result<Held, ToolFailure> {
        let facts = self.facts();
        if facts.checked && facts.git.is_missing() {
            return Err(refused(
                "git isn't installed on this Mac, so no project can be added until the setup \
                 check's fix for it is done",
            ));
        }
        let home = self.home();
        let payload = match args.action {
            ProjectAction::Link => self.resolve_link(args, home.as_deref()).await?,
            ProjectAction::Clone | ProjectAction::Create => {
                self.resolve_new(args, &facts, home.as_deref()).await?
            }
        };
        let description = self.describe(&payload);
        let held_action = self.store_proposal(caller, payload).await?;
        Ok(Held {
            held_action,
            note: format!(
                "Held for the user's confirmation: {description}. Nothing happens until they \
                 approve; you'll be woken with the outcome."
            ),
        })
    }

    /// A clone or a new repository: the location (yours, or the suggested one) and, for a clone,
    /// the URL, with `owner/name` read as a GitHub repository.
    async fn resolve_new(
        &self,
        args: &ProposeProjectArgs,
        facts: &SetupFacts,
        home: Option<&Path>,
    ) -> Result<AddProjectPayload, ToolFailure> {
        let location = match &args.location {
            Some(location) => expand(location, home),
            None => self.suggested_location().await.map_err(|e| failed(&e))?,
        };
        if !location.is_absolute() {
            return Err(refused(format!(
                "the location must be a full path such as ~/Developer, not {:?}",
                args.location.as_deref().unwrap_or_default()
            )));
        }
        let dest = location.join(&args.name);
        if dest.exists() {
            return Err(refused(format!(
                "{} already exists; link it instead, or pick another name or location",
                self.tilde(&dest)
            )));
        }
        let url = if args.action == ProjectAction::Clone {
            let url = args.url.clone().unwrap_or_default();
            let url = if is_shorthand(&url) {
                format!("https://github.com/{url}")
            } else {
                url
            };
            if github_repo(&url).is_some() && facts.checked && !facts.gh_ready() {
                return Err(refused(
                    "cloning from GitHub needs the GitHub CLI, which isn't set up on this Mac; \
                     the setup check offers to install it and log in",
                ));
            }
            Some(url)
        } else {
            None
        };
        Ok(AddProjectPayload {
            action: args.action,
            name: args.name.clone(),
            path: None,
            url,
            location: Some(location.display().to_string()),
        })
    }

    /// Stores the proposal as a pending held action, replacing an older card for the same name,
    /// with an app line in the conversation to show its card under. One transaction.
    async fn store_proposal(
        &self,
        caller: &Caller,
        payload: AddProjectPayload,
    ) -> Result<HeldActionId, ToolFailure> {
        let agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.inner.clock.now();
        let name = payload.name.clone();
        let link_path = payload.path.clone().map(PathBuf::from);
        let stored = payload;
        let outcome = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                if let Some(existing) = projects::active_by_name(&tx, &name)? {
                    return Ok(Err(format!(
                        "a project named {} is already registered ({})",
                        existing.name,
                        existing.path.display()
                    )));
                }
                if let Some(path) = &link_path
                    && let Some(existing) = projects::active_by_path(&tx, path)?
                {
                    return Ok(Err(format!(
                        "that folder is already registered as {}",
                        existing.name
                    )));
                }
                // A newer proposal for the same name replaces the older card.
                for older in HeldService::pending(&tx, HeldKind::AddProject)? {
                    if let HeldPayload::AddProject(p) = &older.payload
                        && p.name.eq_ignore_ascii_case(&name)
                    {
                        HeldService::supersede(&tx, older.id, at)?;
                    }
                }
                let held = HeldService::insert(
                    &tx,
                    &NewHeld {
                        payload: HeldPayload::AddProject(stored),
                        choices: Vec::new(),
                        title: name.clone(),
                        task: None,
                        asked_by: Some(agent),
                        at,
                    },
                )?;
                let message = messages::insert(
                    &tx,
                    &NewMessage {
                        agent,
                        source: MessageSource::App,
                        kind: MessageKind::AppNote,
                        text: "The Quartermaster proposes adding a project.",
                        options: &[],
                        replies_to: None,
                        turn,
                        at,
                    },
                )?;
                HeldService::link_message(&tx, held, message, at)?;
                tx.commit()?;
                Ok(Ok(held))
            })
            .await
            .map_err(|e| failed(&e))?;
        outcome.map_err(refused)
    }

    /// A folder to link: it must be a git repository; its top folder is what's linked.
    async fn resolve_link(
        &self,
        args: &ProposeProjectArgs,
        home: Option<&Path>,
    ) -> Result<AddProjectPayload, ToolFailure> {
        let path = expand(args.path.as_deref().unwrap_or_default(), home);
        if !path.is_absolute() {
            return Err(refused(format!(
                "the folder must be a full path such as ~/Developer/api-server, not {:?}",
                args.path.as_deref().unwrap_or_default()
            )));
        }
        if !path.is_dir() {
            return Err(refused(format!(
                "{} isn't a folder on this Mac",
                tilde(&path, home)
            )));
        }
        let program = self.git_program();
        let probe = path.clone();
        let root = tokio::task::spawn_blocking(move || {
            Repo::open(Git::with_program(program), &probe).map(|r| r.root().to_owned())
        })
        .await
        .map_err(|e| ToolFailure::Failed(format!("the repository check stopped: {e}")))?;
        let root = root.map_err(|_| {
            refused(format!(
                "{} isn't a git repository, so it can't be linked as a project",
                tilde(&path, home)
            ))
        })?;
        Ok(AddProjectPayload {
            action: ProjectAction::Link,
            name: args.name.clone(),
            path: Some(root.display().to_string()),
            url: None,
            location: None,
        })
    }

    /// One plain phrase for a proposal: "link ~/Developer/api-server as api-server".
    fn describe(&self, p: &AddProjectPayload) -> String {
        let at = |dir: &Option<String>| {
            dir.as_deref()
                .map(|d| self.tilde(&Path::new(d).join(&p.name)))
                .unwrap_or_default()
        };
        match p.action {
            ProjectAction::Link => format!(
                "link {} as {}",
                self.tilde(Path::new(p.path.as_deref().unwrap_or_default())),
                p.name
            ),
            ProjectAction::Clone => format!(
                "clone {} into {}",
                p.url.as_deref().unwrap_or_default(),
                at(&p.location)
            ),
            ProjectAction::Create => {
                format!("create a new repository {} at {}", p.name, at(&p.location))
            }
        }
    }

    /// Performs an approved proposal exactly as stored. Idempotent: a project already registered
    /// with this name at this folder counts as done.
    pub async fn add(&self, p: &AddProjectPayload) -> Result<String, HeldError> {
        let facts = self.facts();
        let home = self.home();
        let (origin, folder) = match p.action {
            ProjectAction::Link => (
                ProjectOrigin::Linked,
                PathBuf::from(p.path.clone().unwrap_or_default()),
            ),
            ProjectAction::Clone => (
                ProjectOrigin::Cloned,
                PathBuf::from(p.location.clone().unwrap_or_default()).join(&p.name),
            ),
            ProjectAction::Create => (
                ProjectOrigin::Created,
                PathBuf::from(p.location.clone().unwrap_or_default()).join(&p.name),
            ),
        };
        if !folder.is_absolute() {
            return Err(HeldError::Failed(
                "the stored proposal has no full path; ask for it again".to_owned(),
            ));
        }

        // Already done (a crash after registering, before the card was marked)?
        let name = p.name.clone();
        let check = folder.clone();
        if let Some(existing) = self
            .inner
            .db
            .call(move |conn| projects::active_by_name(conn, &name))
            .await?
        {
            if existing.path == check {
                return Ok(format!(
                    "{} is registered at {}.",
                    existing.name,
                    tilde(&existing.path, home.as_deref())
                ));
            }
            return Err(HeldError::Failed(format!(
                "a project named {} is already registered at {}",
                existing.name,
                tilde(&existing.path, home.as_deref())
            )));
        }

        let make = Materialise {
            action: p.action,
            target: folder,
            url: p.url.clone(),
            gh: facts.paths.gh.clone().filter(|_| facts.gh_ready()),
            git: self.git_program(),
            env: self.inner.env.clone(),
        };
        let repo = tokio::task::spawn_blocking(move || make.run())
            .await
            .map_err(|e| HeldError::Failed(format!("adding the project stopped: {e}")))?
            .map_err(HeldError::Failed)?;

        let delivery = default_delivery(repo.github_remote.as_deref(), repo.has_remote);
        self.register(NewProject {
            name: p.name.clone(),
            path: repo.root.clone(),
            origin,
            default_branch: repo.default_branch.clone(),
            github_remote: repo.github_remote.clone(),
            delivery,
            merge_method: DEFAULT_MERGE_METHOD,
            at: self.inner.clock.now(),
        })
        .await?;
        let verb = match origin {
            ProjectOrigin::Cloned => "Cloned",
            ProjectOrigin::Created => "Created",
            _ => "Linked",
        };
        Ok(format!(
            "{verb} {} at {}: {} from {}.",
            p.name,
            tilde(&repo.root, home.as_deref()),
            delivery_words(delivery),
            repo.default_branch
        ))
    }

    /// Writes the project's row and its `project.added` event.
    async fn register(&self, new: NewProject) -> Result<ProjectId, StoreError> {
        let summary = format!("project {} added ({})", new.name, new.origin);
        // The task numbers its branches already name are reserved before any task can take one.
        // A folder project has no task branches, and its folder may not be a repository.
        let checkouts = match new.delivery {
            DeliveryMode::Folder => Vec::new(),
            _ => vec![new.path.clone()],
        };
        let git = Git::with_program(self.git_program());
        let leftovers = tokio::task::spawn_blocking(move || {
            crate::tasks::startup::branch_task_numbers(&git, &checkouts)
        })
        .await
        .unwrap_or_default();
        self.inner
            .db
            .call(move |conn| {
                qm_store::ids::reserve("tasks", leftovers);
                let tx = conn.transaction()?;
                let id = projects::insert(&tx, &new)?;
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: event_kind::ADDED,
                        agent: None,
                        task: None,
                        turn: None,
                        summary: &summary,
                        payload: serde_json::json!({
                            "project": id,
                            "name": new.name,
                            "origin": new.origin.as_str(),
                        }),
                        at: new.at,
                    },
                )?;
                tx.commit()?;
                Ok(id)
            })
            .await
    }

    /// The project not removed named `name` (any case), or a refusal listing the registered ones.
    async fn named(&self, name: &str) -> Result<ProjectRow, ToolFailure> {
        let wanted = name.to_owned();
        let (found, all) = self
            .inner
            .db
            .call(move |conn| {
                Ok((
                    projects::active_by_name(conn, &wanted)?,
                    projects::active(conn)?,
                ))
            })
            .await
            .map_err(|e| failed(&e))?;
        found.ok_or_else(|| {
            let names: Vec<String> = all.into_iter().map(|p| p.name).collect();
            refused(if names.is_empty() {
                format!("there's no project named {name}; none is registered yet")
            } else {
                format!(
                    "there's no project named {name}; registered: {}",
                    names.join(", ")
                )
            })
        })
    }

    /// The remotes of a project's repository, and whether the default branch named in `branch`
    /// exists in it. A plain folder has no git, so it has neither.
    async fn repo_facts(
        &self,
        project: &ProjectRow,
        branch: Option<String>,
    ) -> Result<(Vec<String>, bool), ToolFailure> {
        if project.delivery == DeliveryMode::Folder {
            return Ok((Vec::new(), false));
        }
        let program = self.git_program();
        let env = self.inner.env.clone();
        let path = project.path.clone();
        tokio::task::spawn_blocking(move || {
            let remotes = git(&program, &env, &path, &["remote"])
                .map(|out| out.lines().map(str::to_owned).collect::<Vec<_>>())
                .unwrap_or_default();
            let known = branch.as_deref().is_none_or(|b| {
                [
                    format!("refs/heads/{b}^{{commit}}"),
                    format!("refs/remotes/origin/{b}^{{commit}}"),
                ]
                .iter()
                .any(|r| {
                    git(
                        &program,
                        &env,
                        &path,
                        &["rev-parse", "--verify", "--quiet", r],
                    )
                    .is_ok()
                })
            });
            (remotes, known)
        })
        .await
        .map_err(|e| ToolFailure::Failed(format!("the project check stopped: {e}")))
    }

    /// `configure_project`: never autonomy or the verification level.
    pub async fn configure(
        &self,
        caller: &Caller,
        args: &ConfigureProjectArgs,
    ) -> Result<Ack, ToolFailure> {
        let project = self.named(&args.project).await?;
        let (remotes, branch_known) = self
            .repo_facts(&project, args.default_branch.clone())
            .await?;

        let said = check_change(&project, args, &remotes, branch_known)?;

        let change = SettingsChange {
            delivery: args.delivery,
            merge_method: args.merge_method,
            default_branch: args.default_branch.clone(),
            files_to_copy: args
                .files_to_copy
                .as_ref()
                .map(|f| f.iter().map(PathBuf::from).collect()),
            setup_command: if args.clear_setup_command {
                Some(None)
            } else {
                args.setup_command.clone().map(Some)
            },
        };
        let id = project.id;
        let agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.inner.clock.now();
        let summary = format!("project {} configured: {}", project.name, said.join(", "));
        let payload = serde_json::json!({
            "project": id,
            "delivery": args.delivery,
            "merge_method": args.merge_method,
            "default_branch": args.default_branch,
            "files_to_copy": args.files_to_copy,
            "setup_command": args.setup_command,
            "clear_setup_command": args.clear_setup_command,
        });
        let event_summary = summary.clone();
        let changed = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let changed = projects::change_settings(&tx, id, &change)?;
                if changed {
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: event_kind::CONFIGURED,
                            agent: Some(agent),
                            task: None,
                            turn,
                            summary: &event_summary,
                            payload,
                            at,
                        },
                    )?;
                }
                tx.commit()?;
                Ok(changed)
            })
            .await
            .map_err(|e| failed(&e))?;
        if !changed {
            return Err(refused(format!("{} was removed meanwhile", project.name)));
        }
        Ok(Ack {
            note: format!(
                "Updated {}: {}. Tasks started from now on use these settings; running tasks \
                 keep theirs.",
                project.name,
                said.join(", ")
            ),
        })
    }

    /// `remove_project`: unregisters it; its folder is never touched.
    pub async fn remove(&self, caller: &Caller, name: &str) -> Result<Ack, ToolFailure> {
        let project = self.named(name).await?;
        if project.origin == ProjectOrigin::Workspace {
            return Err(refused(
                "the personal workspace is built in and can't be removed",
            ));
        }
        let id = project.id;
        let agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.inner.clock.now();
        let summary = format!("project {} removed", project.name);
        let outcome = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let open = projects::open_tasks(&tx, id)?;
                if !open.is_empty() {
                    let list: Vec<String> = open.iter().map(ToString::to_string).collect();
                    return Ok(Err(list));
                }
                if projects::mark_removed(&tx, id, at)? {
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: event_kind::REMOVED,
                            agent: Some(agent),
                            task: None,
                            turn,
                            summary: &summary,
                            payload: serde_json::json!({ "project": id }),
                            at,
                        },
                    )?;
                }
                tx.commit()?;
                Ok(Ok(()))
            })
            .await
            .map_err(|e| failed(&e))?;
        if let Err(open) = outcome {
            return Err(refused(format!(
                "{} still has open tasks ({}); stop or close them first",
                project.name,
                open.join(", ")
            )));
        }
        Ok(Ack {
            note: format!(
                "Removed {} from the project list. Its folder at {} is untouched.",
                project.name,
                self.tilde(&project.path)
            ),
        })
    }

    /// Registers the personal workspace once, as `personal`, in folder mode: its folder is the one
    /// `[workspace] folder` names, else `<data dir>/workspace`, which is made here. The folder
    /// isn't checked: a task that starts while it's missing fails and says so. Git never runs here.
    pub async fn ensure_workspace(&self) -> Result<Option<ProjectId>, HeldError> {
        let config = self.inner.config.borrow().clone();
        let folder = workspace_folder(&config, &self.inner.data_dir, self.home().as_deref());
        if folder == self.inner.data_dir.workspace() {
            let dir = folder.clone();
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir))
                .await
                .map_err(|e| HeldError::Failed(format!("setting up the workspace stopped: {e}")))?
                .map_err(|e| {
                    HeldError::Failed(format!(
                        "the personal workspace folder couldn't be made: {e}"
                    ))
                })?;
        }
        let at = self.inner.clock.now();
        let id = self
            .inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                if let Some(existing) = projects::workspace(&tx)? {
                    // Settings moved the folder while the app was off: follow it, if nothing is open.
                    if existing.path != folder {
                        if projects::open_tasks(&tx, existing.id)?.is_empty() {
                            projects::set_path(&tx, existing.id, &folder)?;
                        } else {
                            tracing::warn!("the personal folder differs from settings; personal tasks are open, so it stays");
                        }
                    }
                    tx.commit()?;
                    return Ok(existing.id);
                }
                let taken = projects::active_by_name(&tx, WORKSPACE_NAME)?.is_some();
                let name = if taken {
                    format!("{WORKSPACE_NAME}-workspace")
                } else {
                    WORKSPACE_NAME.to_owned()
                };
                let id = projects::insert(
                    &tx,
                    &NewProject {
                        name,
                        path: folder,
                        origin: ProjectOrigin::Workspace,
                        // A plain folder has no branches: nothing names a default one.
                        default_branch: String::new(),
                        github_remote: None,
                        delivery: DeliveryMode::Folder,
                        merge_method: DEFAULT_MERGE_METHOD,
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(id)
            })
            .await?;
        Ok(Some(id))
    }

    /// Your `SetProjectAutonomy`: the one way autonomy changes. No tool reaches this; it comes
    /// only from the UI. Returns whether anything changed.
    pub async fn set_autonomy(&self, name: &str, autonomy: Autonomy) -> Result<bool, StoreError> {
        let name = name.to_owned();
        let at = self.inner.clock.now();
        self.inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let Some(project) = projects::active_by_name(&tx, &name)? else {
                    return Ok(false);
                };
                let changed = projects::set_autonomy(&tx, project.id, autonomy)?;
                if changed {
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: event_kind::AUTONOMY,
                            agent: None,
                            task: None,
                            turn: None,
                            summary: &format!("you turned autonomy {autonomy} for {name}"),
                            payload: serde_json::json!({ "project": name, "autonomy": autonomy }),
                            at,
                        },
                    )?;
                }
                tx.commit()?;
                Ok(changed)
            })
            .await
    }

    /// The projects pane's rows.
    async fn rows(&self) -> Result<Vec<qm_types::api::ProjectRow>, StoreError> {
        self.inner
            .db
            .call(|conn| {
                let working: HashSet<ProjectId> =
                    projects::with_work_running(conn)?.into_iter().collect();
                Ok(projects::active(conn)?
                    .into_iter()
                    .map(|p| {
                        let mut row =
                            qm_types::api::ProjectRow::new(p.name, p.delivery, p.autonomy);
                        row.active = working.contains(&p.id);
                        row
                    })
                    .collect())
            })
            .await
    }
}

/// Checks a settings change against the project's facts and says what it changes, one phrase per
/// setting.
fn check_change(
    project: &ProjectRow,
    args: &ConfigureProjectArgs,
    remotes: &[String],
    branch_known: bool,
) -> Result<Vec<String>, ToolFailure> {
    let mut said: Vec<String> = Vec::new();
    let effective = args.delivery.unwrap_or(project.delivery);
    if let Some(delivery) = args.delivery {
        if project.origin == ProjectOrigin::Workspace && delivery != DeliveryMode::Folder {
            return Err(refused(
                "the personal workspace is a plain folder: its changes go straight into it, with \
                 no branches or remote",
            ));
        }
        if delivery == DeliveryMode::Folder && project.origin != ProjectOrigin::Workspace {
            return Err(refused(format!(
                "{} is a git project, so it delivers through branches or pull requests; only the \
                 personal workspace is a plain folder",
                project.name
            )));
        }
        if delivery == DeliveryMode::DirectPr && project.github_remote.is_none() {
            return Err(refused(format!(
                "{} has no GitHub remote, so it can't use direct pull requests",
                project.name
            )));
        }
        if delivery == DeliveryMode::BranchOnly
            && project.origin != ProjectOrigin::Workspace
            && remotes.is_empty()
        {
            return Err(refused(format!(
                "{} has no remote to push a branch to, so it can't be branch-only",
                project.name
            )));
        }
        said.push(format!("delivery {}", delivery_words(delivery)));
    }
    if let Some(method) = args.merge_method {
        said.push(format!("merge method {method}"));
    }
    if let Some(branch) = &args.default_branch {
        if effective == DeliveryMode::Folder {
            return Err(refused(format!(
                "{} is a plain folder, so it has no default branch",
                project.name
            )));
        }
        if !branch_known {
            return Err(refused(format!(
                "{} has no branch named {branch}, locally or on origin",
                project.name
            )));
        }
        said.push(format!("default branch {branch}"));
    }
    if let Some(files) = &args.files_to_copy {
        if effective == DeliveryMode::Folder && !files.is_empty() {
            return Err(refused(format!(
                "{} is a plain folder, so there's no worktree to copy files into",
                project.name
            )));
        }
        said.push(if files.is_empty() {
            "no files to copy".to_owned()
        } else {
            format!("files to copy {}", files.join(", "))
        });
    }
    if let Some(command) = &args.setup_command {
        said.push(format!("setup command `{command}`"));
    }
    if args.clear_setup_command {
        said.push("no setup command".to_owned());
    }
    Ok(said)
}

/// Carries out an approved `add_project` held action exactly as stored.
#[derive(Debug)]
pub struct AddProjectPerformer {
    projects: ProjectService,
}

impl AddProjectPerformer {
    pub fn new(projects: ProjectService) -> Self {
        Self { projects }
    }
}

impl HeldPerformer for AddProjectPerformer {
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>> {
        Box::pin(async move {
            let HeldPayload::AddProject(payload) = &held.payload else {
                return Err(HeldError::Failed(
                    "this card isn't a project proposal".to_owned(),
                ));
            };
            if *answer != HeldAnswer::Approve {
                return Err(Refusal::new("a project proposal takes Approve or Reject").into());
            }
            let note = self.projects.add(payload).await?;
            Ok(HeldOutcome::new(note))
        })
    }

    fn card(&self, held: &LoadedHeld) -> HeldCard {
        let HeldPayload::AddProject(p) = &held.payload else {
            return HeldCard::default();
        };
        let text = |s: String| Styled::new(s, Tone::Text);
        let dim = |s: &str| Styled::new(s, Tone::Dim);
        let home = self.projects.home();
        let place = |dir: &Option<String>| {
            dir.as_deref()
                .map(|d| tilde(&Path::new(d).join(&p.name), home.as_deref()))
                .unwrap_or_default()
        };
        let (note, lines) = match p.action {
            ProjectAction::Link => (
                "· link",
                vec![
                    CardLine::new(
                        2,
                        vec![
                            dim("Link "),
                            text(tilde(
                                Path::new(p.path.as_deref().unwrap_or_default()),
                                home.as_deref(),
                            )),
                        ],
                    ),
                    CardLine::new(2, vec![dim("Nothing is moved or copied.")]),
                ],
            ),
            ProjectAction::Clone => (
                "· clone",
                vec![
                    CardLine::new(
                        2,
                        vec![dim("Clone "), text(p.url.clone().unwrap_or_default())],
                    ),
                    CardLine::new(2, vec![dim("into "), text(place(&p.location))]),
                ],
            ),
            ProjectAction::Create => (
                "· create",
                vec![CardLine::new(
                    2,
                    vec![dim("Create a new repository at "), text(place(&p.location))],
                )],
            ),
        };
        // A clone's or a new project's folder can be changed before anything is created.
        let buttons = p
            .location
            .as_deref()
            .filter(|_| p.action != ProjectAction::Link)
            .map(|location| {
                CardButton::new(
                    "Change location",
                    'c',
                    CardAction::ChangeLocation {
                        location: tilde(Path::new(location), home.as_deref()),
                    },
                )
            })
            .into_iter()
            .collect();
        HeldCard {
            note: Some(note.to_owned()),
            lines,
            buttons,
        }
    }
}

impl ProjectService {
    /// Your folder for a pending clone or new project: checked like a proposal's location, then
    /// written into the held action's stored payload, which is what Approve then performs.
    pub async fn change_location(
        &self,
        held: &HeldService,
        id: HeldActionId,
        location: &str,
    ) -> Result<(), HeldError> {
        let row = self
            .inner
            .db
            .call(move |conn| qm_store::held::load_held(conn, id))
            .await?;
        let HeldPayload::AddProject(before) = &row.payload else {
            return Err(Refusal::new("only a project proposal has a location").into());
        };
        if before.action == ProjectAction::Link {
            return Err(Refusal::new(
                "a linked project stays where it is; its folder can't change",
            )
            .into());
        }
        let home = self.home();
        let folder = expand(location, home.as_deref());
        if !folder.is_absolute() {
            return Err(Refusal::new(format!(
                "the location must be a full path such as ~/Developer, not {location:?}"
            ))
            .into());
        }
        let dest = folder.join(&before.name);
        if dest.exists() {
            return Err(Refusal::new(format!(
                "{} already exists; pick another location",
                self.tilde(&dest)
            ))
            .into());
        }
        let mut after = before.clone();
        after.location = Some(folder.display().to_string());
        let summary = format!(
            "you changed where {} goes to {}",
            before.name,
            self.tilde(&dest)
        );
        held.amend(
            id,
            HeldPayload::AddProject(before.clone()),
            HeldPayload::AddProject(after),
            summary,
        )
        .await
    }
}

/// The API's end of the registry: the projects pane.
#[derive(Debug)]
pub struct ProjectsPart {
    projects: ProjectService,
    held: HeldService,
}

impl ProjectsPart {
    pub fn new(projects: ProjectService, held: HeldService) -> Self {
        Self { projects, held }
    }

    /// Registers the personal workspace in the background, then asks for a fresh snapshot.
    pub fn start(&self, conversation: Arc<Conversation>) {
        let projects = self.projects.clone();
        tokio::spawn(async move {
            match projects.ensure_workspace().await {
                Ok(Some(_)) => conversation.changed(),
                Ok(None) => {}
                Err(e) => tracing::error!(error = %e, "couldn't set up the personal workspace"),
            }
        });
    }
}

impl ApiPart for ProjectsPart {
    fn handle<'a>(&'a self, command: &'a qm_types::api::Command) -> Option<BoxFuture<'a, Handled>> {
        if let qm_types::api::Command::SetProjectAutonomy { project, autonomy } = command {
            return Some(Box::pin(async move {
                match self.projects.set_autonomy(project, *autonomy).await {
                    Ok(true) => Handled::Changed,
                    Ok(false) => Handled::Unchanged,
                    Err(e) => {
                        tracing::error!(error = %e, "couldn't change the project's autonomy");
                        Handled::Changed
                    }
                }
            }));
        }
        let qm_types::api::Command::ChangeHeldLocation { held, location } = command else {
            return None;
        };
        Some(Box::pin(async move {
            match self
                .projects
                .change_location(&self.held, *held, location)
                .await
            {
                Ok(()) => {}
                Err(HeldError::Store(e)) => {
                    tracing::error!(error = %e, %held, "couldn't change the proposal's location");
                }
                Err(e) => self.held.show_failure(*held, e.to_string()),
            }
            Handled::Changed
        }))
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match self.projects.rows().await {
                Ok(rows) => snapshot.projects = rows,
                Err(e) => tracing::error!(error = %e, "couldn't read the projects for the UI"),
            }
        })
    }
}
