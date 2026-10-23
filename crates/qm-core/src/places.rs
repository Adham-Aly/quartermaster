//! Where work runs: the project a folder belongs to, and its registration.
//!
//! A project is a folder: the enclosing git root, else a listed plain-folder project that contains
//! it, else the folder itself. The rule never climbs to your home folder, and home never contains
//! another project. The project is registered in the transaction that records the task: the active
//! row at that path is used, a retired one comes back with its settings, or a new one is added.
//! It is retired again when its last open task closes ([`retire_if_idle`]).

use crate::projects::{
    DEFAULT_MERGE_METHOD, RepoFacts, default_delivery, event_kind, expand, repo_facts,
};
use jiff::Timestamp;
use qm_store::StoreError;
use qm_store::events::{self, NewEvent};
use qm_store::projects::{self, NewProject, ProjectRow};
use qm_types::domain::{Autonomy, DeliveryMode, ProjectOrigin, VerificationLevel};
use qm_types::ids::ProjectId;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The name of the project for work in your home folder.
pub const HOME_NAME: &str = "home";

/// A folder, resolved: the project's folder and, for a repository, its facts.
#[derive(Clone, Debug)]
pub(crate) struct Place {
    /// The project's folder: a git root, or a plain folder.
    pub root: PathBuf,
    /// The folder named, where an in-place task works.
    pub folder: PathBuf,
    /// The repository's facts, when `root` is a git root.
    pub repo: Option<RepoFacts>,
}

/// What resolving a folder needs to know, all outside the database.
pub(crate) struct Resolver<'a> {
    pub home: &'a Path,
    pub data_dir: &'a Path,
    pub git: &'a Path,
    pub env: &'a BTreeMap<String, String>,
}

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

impl Resolver<'_> {
    /// Resolves `text` (absolute, or from `~`) to its place, given the folders of the listed
    /// plain-folder projects. Blocking: it runs git.
    pub(crate) fn resolve(&self, text: &str, plain: &[PathBuf]) -> Result<Place, String> {
        let text = text.trim();
        if !(text.starts_with('/') || text == "~" || text.starts_with("~/")) {
            return Err(format!(
                "{text} isn't an absolute path: write it from / or from ~"
            ));
        }
        let path = expand(text, Some(self.home));
        let folder = path
            .canonicalize()
            .map_err(|_| format!("{} isn't there", path.display()))?;
        if !folder.is_dir() {
            return Err(format!("{} isn't a folder", path.display()));
        }
        let data = real(self.data_dir);
        if folder.starts_with(&data) {
            return Err(format!(
                "{} is inside the app's own data folder, so no task works there",
                path.display()
            ));
        }
        Ok(self.place_of(folder, plain))
    }

    fn place_of(&self, folder: PathBuf, plain: &[PathBuf]) -> Place {
        let home = real(self.home);
        if folder == home {
            return Place {
                root: folder.clone(),
                folder,
                repo: None,
            };
        }
        // The enclosing git root, unless it is home or above it.
        if let Ok(mut facts) = repo_facts(self.git, self.env, &folder) {
            let root = real(&facts.root);
            if !home.starts_with(&root) {
                facts.root.clone_from(&root);
                return Place {
                    root,
                    folder,
                    repo: Some(facts),
                };
            }
        }
        // A listed plain folder that contains it, the nearest first; home never counts.
        let listed = plain
            .iter()
            .map(|p| real(p))
            .filter(|p| *p != home && folder.starts_with(p))
            .max_by_key(|p| p.components().count());
        Place {
            root: listed.unwrap_or_else(|| folder.clone()),
            folder,
            repo: None,
        }
    }
}

/// The folders of the listed plain-folder projects.
pub(crate) fn plain_folders(conn: &Connection) -> Result<Vec<PathBuf>, StoreError> {
    Ok(projects::active(conn)?
        .into_iter()
        .filter(|p| p.delivery == DeliveryMode::Folder)
        .map(|p| p.path)
        .collect())
}

fn new_project(conn: &Connection, place: &Place, home: &Path, at: Timestamp) -> NewProject {
    let base = if real(&place.root) == real(home) {
        HOME_NAME.to_owned()
    } else {
        place
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("folder")
            .to_owned()
    };
    let name = projects::free_name(conn, &base).unwrap_or(base);
    let (default_branch, github_remote, delivery) = match &place.repo {
        Some(repo) => (
            repo.default_branch.clone(),
            repo.github_remote.clone(),
            default_delivery(repo.github_remote.as_deref(), repo.has_remote),
        ),
        None => (String::new(), None, DeliveryMode::Folder),
    };
    NewProject {
        name,
        path: place.root.clone(),
        origin: ProjectOrigin::Linked,
        default_branch,
        github_remote,
        delivery,
        merge_method: DEFAULT_MERGE_METHOD,
        at,
    }
}

/// The project `place` would get, without writing anything: the row at its path (a retired one as
/// it would come back), else a new row's values with a placeholder ID. [`register`] then writes it.
pub(crate) fn preview(
    conn: &Connection,
    place: &Place,
    home: &Path,
    at: Timestamp,
) -> Result<ProjectRow, StoreError> {
    if let Some(row) = projects::active_by_path(conn, &place.root)? {
        return Ok(row);
    }
    if let Some(mut row) = projects::retired_by_path(conn, &place.root)? {
        row.removed = false;
        return Ok(row);
    }
    let new = new_project(conn, place, home, at);
    Ok(ProjectRow {
        id: ProjectId::new(0),
        name: new.name,
        path: new.path,
        origin: new.origin,
        default_branch: new.default_branch,
        github_remote: new.github_remote,
        delivery: new.delivery,
        autonomy: Autonomy::Normal,
        verification: VerificationLevel::Standard,
        merge_method: new.merge_method,
        files_to_copy: Vec::new(),
        setup_command: None,
        removed: false,
    })
}

fn added_event(
    conn: &Connection,
    id: ProjectId,
    name: &str,
    summary: &str,
    at: Timestamp,
) -> Result<(), StoreError> {
    events::insert(
        conn,
        &NewEvent {
            kind: event_kind::ADDED,
            agent: None,
            task: None,
            turn: None,
            summary,
            payload: serde_json::json!({ "project": id, "name": name }),
            at,
        },
    )
    .map(drop)
}

/// Registers the project for `place`, inside the caller's transaction: the active row at its path,
/// else the retired one brought back with its settings, else a new row.
pub(crate) fn register(
    conn: &Connection,
    place: &Place,
    home: &Path,
    at: Timestamp,
) -> Result<ProjectRow, StoreError> {
    if let Some(row) = projects::active_by_path(conn, &place.root)? {
        return Ok(row);
    }
    let id = if let Some(row) = projects::retired_by_path(conn, &place.root)? {
        projects::revive(conn, row.id)?;
        let summary = format!("project {} is back", row.name);
        added_event(conn, row.id, &row.name, &summary, at)?;
        row.id
    } else {
        let new = new_project(conn, place, home, at);
        let id = projects::insert(conn, &new)?;
        let summary = format!("project {} added", new.name);
        added_event(conn, id, &new.name, &summary, at)?;
        id
    };
    projects::load(conn, id)?.ok_or_else(|| StoreError::Invalid {
        column: "projects.id",
        message: format!("project {id} vanished while it was registered"),
    })
}

/// Retires `project` once it has no open task, inside the caller's transaction. Its folder and
/// history are untouched; work there again brings it back. Returns whether it was retired.
pub(crate) fn retire_if_idle(
    conn: &Connection,
    project: ProjectId,
    at: Timestamp,
) -> Result<bool, StoreError> {
    if !projects::open_tasks(conn, project)?.is_empty() {
        return Ok(false);
    }
    let Some(row) = projects::load(conn, project)? else {
        return Ok(false);
    };
    if !projects::mark_removed(conn, project, at)? {
        return Ok(false);
    }
    events::insert(
        conn,
        &NewEvent {
            kind: event_kind::REMOVED,
            agent: None,
            task: None,
            turn: None,
            summary: &format!("project {} retired: its last task closed", row.name),
            payload: serde_json::json!({ "project": project, "name": row.name }),
            at,
        },
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver<'a>(
        home: &'a Path,
        data: &'a Path,
        env: &'a BTreeMap<String, String>,
    ) -> Resolver<'a> {
        Resolver {
            home,
            data_dir: data,
            git: Path::new("git"),
            env,
        }
    }

    fn env() -> BTreeMap<String, String> {
        std::env::vars().filter(|(k, _)| k == "PATH").collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qm-places-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        real(&dir)
    }

    #[test]
    fn a_plain_folder_is_its_own_project_and_home_is_home() {
        let base = scratch("plain");
        let home = base.join("home");
        let inbox = home.join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let env = env();
        let r = resolver(&home, &data, &env);
        let place = r.resolve(inbox.to_str().unwrap(), &[]).unwrap();
        assert_eq!(place.root, inbox);
        assert!(place.repo.is_none());
        assert_eq!(r.resolve("~", &[]).unwrap().root, home);
        // A listed plain folder contains its subfolders; home never does.
        let deep = inbox.join("a");
        std::fs::create_dir_all(&deep).unwrap();
        let listed = r
            .resolve("~/inbox/a", &[inbox.clone(), home.clone()])
            .unwrap();
        assert_eq!(listed.root, inbox);
        assert_eq!(listed.folder, deep);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_inside_a_repository_is_labelled_with_the_repository() {
        let base = scratch("git");
        let home = base.join("home");
        let repo = home.join("site");
        let deep = repo.join("src/app");
        std::fs::create_dir_all(&deep).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let env = env();
        let place = resolver(&home, &data, &env)
            .resolve(deep.to_str().unwrap(), &[])
            .unwrap();
        assert_eq!(place.root, repo);
        assert_eq!(place.folder, deep);
        assert!(place.repo.is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn relative_missing_and_app_folders_are_refused() {
        let base = scratch("refuse");
        let data = base.join("data");
        std::fs::create_dir_all(data.join("tasks")).unwrap();
        let env = env();
        let r = resolver(&base, &data, &env);
        assert!(r.resolve("inbox", &[]).is_err());
        assert!(r.resolve("~/nowhere", &[]).is_err());
        assert!(
            r.resolve(data.join("tasks").to_str().unwrap(), &[])
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
