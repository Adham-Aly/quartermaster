//! The project registry.
//!
//! A project is a folder: a git repository, or a plain folder. Projects aren't added or removed by
//! hand: one appears when work starts in its folder and is retired when its last open task closes
//! (`crate::places`). A retired project keeps its settings, which come back with it.
//!
//! - **`clone_repository`** ([`ProjectService::clone_repository`]) puts a repository on disk, with
//!   no card; a task started in it then registers it.
//! - **`find_project_folder`** looks only when asked, in the search roots from settings
//!   ([`find_folders`]).
//! - **`configure_project` can't change autonomy or the verification level** (its arguments have no
//!   field for either), and checks what it can change against the facts: a direct-PR project needs
//!   a GitHub remote, a branch-only one a remote, and a plain folder stays a plain folder until it
//!   becomes a repository (its kind is read again first, as when work starts there).
//!
//! [`ProjectsPart`] fills the Bridge's projects pane.

use crate::api::{ApiPart, Handled};
use crate::setup_check::SetupFacts;
use qm_git::{Git, Repo};
use qm_store::StoreError;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::projects::{self, ProjectRow, SettingsChange};
use qm_tools::defs::{Ack, ConfigureProjectArgs};
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::BoxFuture;
use qm_types::api::Snapshot;
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::data_dir::DataDir;
use qm_types::domain::{Autonomy, DeliveryMode, MergeMethod};
use qm_types::error::Refusal;
use qm_types::ids::ProjectId;
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
/// The merge method a new project starts with; you change it with `configure_project`.
pub const DEFAULT_MERGE_METHOD: MergeMethod = MergeMethod::Squash;

/// Event kinds the registry writes.
pub mod event_kind {
    pub const CONFIGURED: &str = "project.configured";
    pub const REMOVED: &str = "project.removed";
    pub const ADDED: &str = "project.added";
    /// A project's folder changed kind: a plain folder became a repository, or a repository
    /// gained or lost its GitHub remote. New tasks follow; running ones keep how they started.
    pub const KIND: &str = "project.kind";
    /// You set a project's autonomy to normal or auto (never an agent).
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

/// Folders named `name` (any case), up to [`SEARCH_DEPTH`] levels under each root, best first: an
/// exact-case match before others, a git repository before a plain folder, shallower before
/// deeper. A plain folder is a project too, so it is found as well. Symbolic links to
/// folders are followed once each; nothing is read beyond the depth.
pub fn find_folders(roots: &[PathBuf], name: &str) -> Vec<PathBuf> {
    let wanted = name.to_lowercase();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut found: Vec<(bool, bool, usize, PathBuf)> = Vec::new();
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
                    if file_name.to_lowercase() == wanted {
                        let plain = !path.join(".git").exists();
                        found.push((file_name != name, plain, depth, path.clone()));
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
        .map(|(_, _, _, p)| p)
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

pub(crate) fn delivery_words(mode: DeliveryMode) -> &'static str {
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
#[derive(Clone, Debug)]
pub(crate) struct RepoFacts {
    pub(crate) root: PathBuf,
    pub(crate) default_branch: String,
    pub(crate) github_remote: Option<String>,
    pub(crate) has_remote: bool,
}

pub(crate) fn repo_facts(
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

/// Clones `url` into `target`, through `gh` for a GitHub repository when it's logged in, else
/// through git. Blocking.
fn clone_into(
    target: &Path,
    url: &str,
    gh: Option<&Path>,
    program: &Path,
    env: &BTreeMap<String, String>,
) -> Result<RepoFacts, String> {
    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }
    let parent = target.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("couldn't create {}: {e}", parent.display()))?;
    let dest = target.display().to_string();
    match (github_repo(url), gh) {
        (Some(repo), Some(gh)) => {
            let out = std::process::Command::new(gh)
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
            git(program, env, parent, &["clone", "--quiet", url, &dest])?;
        }
    }
    repo_facts(program, env, target)
}

/// The folder name a clone of `url` gets: its last path part, without `.git`.
fn clone_name(url: &str) -> Option<String> {
    let last = url
        .trim()
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()?
        .trim_end_matches(".git");
    (!last.is_empty() && last != "." && last != "..").then(|| last.to_owned())
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

    /// `clone_repository`: clones `url` into `location` (else the first search root, else your
    /// home folder) and returns the folder. No card: nothing is registered until work starts there.
    pub async fn clone_repository(
        &self,
        url: &str,
        location: Option<&str>,
    ) -> Result<Ack, ToolFailure> {
        let facts = self.facts();
        if facts.checked && facts.git.is_missing() {
            return Err(refused(
                "git isn't installed on this Mac, so nothing can be cloned until the setup \
                 check's fix for it is done",
            ));
        }
        let home = self.home();
        let url = url.trim();
        let url = if is_shorthand(url) {
            format!("https://github.com/{url}")
        } else {
            url.to_owned()
        };
        let Some(name) = clone_name(&url) else {
            return Err(refused(format!("{url} doesn't name a repository to clone")));
        };
        let location = if let Some(l) = location.map(str::trim).filter(|l| !l.is_empty()) {
            expand(l, home.as_deref())
        } else {
            let first = self
                .inner
                .config
                .borrow()
                .projects
                .search_roots
                .first()
                .cloned();
            match (first, home.clone()) {
                (Some(root), _) => expand(&root, home.as_deref()),
                (None, Some(h)) => h,
                (None, None) => {
                    return Err(refused("name a location: there's no home folder to use"));
                }
            }
        };
        if !location.is_absolute() {
            return Err(refused(format!(
                "the location must be a full path such as ~/Developer, not {}",
                location.display()
            )));
        }
        if location.starts_with(self.inner.data_dir.root()) {
            return Err(refused(
                "that location is inside the app's own data folder; pick another",
            ));
        }
        let dest = location.join(&name);
        if dest.exists() {
            return Err(refused(format!(
                "{} already exists; start work in it, or name another location",
                self.tilde(&dest)
            )));
        }
        let gh = facts.paths.gh.clone().filter(|_| facts.gh_ready());
        let program = self.git_program();
        let env = self.inner.env.clone();
        let target = dest.clone();
        let cloned = tokio::task::spawn_blocking(move || {
            clone_into(&target, &url, gh.as_deref(), &program, &env)
        })
        .await
        .map_err(|e| ToolFailure::Failed(format!("the clone stopped: {e}")))?
        .map_err(refused)?;
        Ok(Ack {
            note: format!(
                "Cloned into {} ({} from {}). Start work there with start_crewmate; the project \
                 appears when it does.",
                self.tilde(&cloned.root),
                if cloned.github_remote.is_some() {
                    "on GitHub"
                } else {
                    "a git repository"
                },
                if cloned.default_branch.is_empty() {
                    "no branch yet"
                } else {
                    &cloned.default_branch
                },
            ),
        })
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

    /// `project` with its folder's kind read again and any change recorded
    /// ([`crate::places::kind_change`]): a plain folder may have become a repository since.
    async fn refresh_kind(&self, project: ProjectRow) -> Result<ProjectRow, ToolFailure> {
        let Some(home) = self.home() else {
            return Ok(project);
        };
        let program = self.git_program();
        let env = self.inner.env.clone();
        let data = self.inner.data_dir.root().to_owned();
        let root = project.path.clone();
        let place = tokio::task::spawn_blocking(move || {
            crate::places::Resolver {
                home: &home,
                data_dir: &data,
                git: &program,
                env: &env,
            }
            .at_root(root)
        })
        .await
        .map_err(|e| ToolFailure::Failed(format!("the project check stopped: {e}")))?;
        if crate::places::kind_change(&project, &place).is_none() {
            return Ok(project);
        }
        let home = self.home().unwrap_or_default();
        let at = self.inner.clock.now();
        self.inner
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let row = crate::places::register(&tx, &place, &home, at)?;
                tx.commit()?;
                Ok(row)
            })
            .await
            .map_err(|e| failed(&e))
    }

    /// `configure_project`: never autonomy or the verification level.
    pub async fn configure(
        &self,
        caller: &Caller,
        args: &ConfigureProjectArgs,
    ) -> Result<Ack, ToolFailure> {
        let project = self.named(&args.project).await?;
        let project = self.refresh_kind(project).await?;
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
                            summary: &format!("you set autonomy for {name} to {autonomy}"),
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
        if project.delivery == DeliveryMode::Folder && delivery != DeliveryMode::Folder {
            return Err(refused(format!(
                "{} is a plain folder: its changes go straight into it, with no branches or \
                 remote. When the user asks, a crewmate can make it a git or GitHub repository; \
                 tasks started after that use worktrees",
                project.name
            )));
        }
        if delivery == DeliveryMode::Folder && project.delivery != DeliveryMode::Folder {
            return Err(refused(format!(
                "{} is a git project, so it delivers through branches or pull requests; for work \
                 straight in its checkout, start a task in place when the user asks for it",
                project.name
            )));
        }
        if delivery == DeliveryMode::DirectPr && project.github_remote.is_none() {
            return Err(refused(format!(
                "{} has no GitHub remote, so it can't use direct pull requests",
                project.name
            )));
        }
        if delivery == DeliveryMode::BranchOnly && remotes.is_empty() {
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

/// The API's end of the registry: the projects pane.
#[derive(Debug)]
pub struct ProjectsPart {
    projects: ProjectService,
    /// The tasks, told when autonomy changes so open plans follow it.
    tasks: Option<crate::tasks::TaskService>,
}

impl ProjectsPart {
    pub fn new(projects: ProjectService) -> Self {
        Self {
            projects,
            tasks: None,
        }
    }

    /// Tells `tasks` of each autonomy change, so open plans follow it.
    #[must_use]
    pub fn with_tasks(mut self, tasks: crate::tasks::TaskService) -> Self {
        self.tasks = Some(tasks);
        self
    }
}

impl ApiPart for ProjectsPart {
    fn handle<'a>(&'a self, command: &'a qm_types::api::Command) -> Option<BoxFuture<'a, Handled>> {
        if let qm_types::api::Command::SetProjectAutonomy { project, autonomy } = command {
            return Some(Box::pin(async move {
                match self.projects.set_autonomy(project, *autonomy).await {
                    Ok(true) => {
                        if let Some(tasks) = &self.tasks {
                            tasks.autonomy_changed(project).await;
                        }
                        Handled::Changed
                    }
                    Ok(false) => Handled::Unchanged,
                    Err(e) => {
                        tracing::error!(error = %e, "couldn't change the project's autonomy");
                        Handled::Changed
                    }
                }
            }));
        }
        None
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

#[cfg(test)]
#[path = "projects_tests.rs"]
mod tests;
