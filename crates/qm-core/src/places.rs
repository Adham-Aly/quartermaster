//! Where work runs: the project a folder belongs to, and its registration.
//!
//! A project is a folder: the enclosing git root, else a listed plain-folder project that contains
//! it, else the folder itself. The rule never climbs to your home folder, and home never contains
//! another project. Nobody adds or removes a project by hand: it is registered in the transaction
//! that records a task, and retired again when its last open task closes ([`retire_if_idle`]).
//! Registering uses the active row at that path, else brings a retired one back with its
//! settings, else adds a new one.
//!
//! **What a project is decides how its work is delivered**, and it is read again every time work
//! starts there ([`kind_change`]):
//!
//! | The folder is | Tasks work in | Settling |
//! |---|---|---|
//! | a GitHub repository | worktrees | merges the pull request |
//! | a local-only repository | worktrees | merges the branch into your checkout, after your approval |
//! | a repository with another remote | worktrees | pushes the branch |
//! | a plain folder | the folder itself | nothing to merge: the edits are final |
//!
//! So a plain folder you turn into a repository (or give a GitHub remote) changes kind for the
//! tasks started afterwards; a task already working in it keeps working the way it started.

use crate::projects::{
    DEFAULT_MERGE_METHOD, RepoFacts, default_delivery, event_kind, expand, repo_facts,
};
use jiff::Timestamp;
use qm_store::StoreError;
use qm_store::events::{self, NewEvent};
use qm_store::projects::{self, KindChange, NewProject, ProjectRow};
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

    /// Makes the folder `text` names (absolute, or from `~`) with any missing parents, when the
    /// user asked for a new one. An existing folder is left as it is. Refused inside the app's own
    /// data folder, like any place. Blocking.
    pub(crate) fn create(&self, text: &str) -> Result<(), String> {
        let text = text.trim();
        if !(text.starts_with('/') || text == "~" || text.starts_with("~/")) {
            return Err(format!(
                "{text} isn't an absolute path: write it from / or from ~"
            ));
        }
        let path = expand(text, Some(self.home));
        if path.is_dir() {
            return Ok(());
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "{text} has a .. in it: name the new folder by its own path"
            ));
        }
        // Judge where it would land by its nearest existing folder, symlinks resolved.
        let existing = path
            .ancestors()
            .find(|a| a.exists())
            .map_or_else(|| PathBuf::from("/"), real);
        let data = real(self.data_dir);
        if existing.starts_with(&data) {
            return Err(format!(
                "{} would be inside the app's own data folder, so no task works there",
                path.display()
            ));
        }
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("{} couldn't be made: {e}", path.display()))
    }

    /// The place of a listed project's own folder, its repository facts read again (they may
    /// have changed since it was registered). Blocking: it runs git.
    pub(crate) fn at_root(&self, root: PathBuf) -> Place {
        let repo = repo_facts(self.git, self.env, &root)
            .ok()
            .filter(|facts| real(&facts.root) == real(&root) && real(&root) != real(self.home));
        Place {
            folder: root.clone(),
            root,
            repo,
        }
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

/// How `row`'s folder has changed kind, as `place` (read at the project's own folder) shows it
/// now: a plain folder that became a repository, a repository that gained or lost its GitHub
/// remote, or one whose `.git` is gone. The delivery mode goes back to the default for the new
/// kind; nothing changes when the kind is the same, so a mode set with `configure_project` stays.
pub(crate) fn kind_change(row: &ProjectRow, place: &Place) -> Option<KindChange> {
    if real(&place.root) != real(&row.path) {
        return None;
    }
    let was_folder = row.delivery == DeliveryMode::Folder;
    match &place.repo {
        Some(repo) => {
            if !was_folder && repo.github_remote == row.github_remote {
                return None;
            }
            let default_branch = if was_folder || row.default_branch.is_empty() {
                repo.default_branch.clone()
            } else {
                row.default_branch.clone()
            };
            Some(KindChange {
                delivery: default_delivery(repo.github_remote.as_deref(), repo.has_remote),
                default_branch,
                github_remote: repo.github_remote.clone(),
            })
        }
        // Only when the repository is really gone, never because git couldn't be read.
        None if !was_folder && !place.root.join(".git").exists() => Some(KindChange {
            delivery: DeliveryMode::Folder,
            default_branch: String::new(),
            github_remote: None,
        }),
        None => None,
    }
}

fn apply(row: &mut ProjectRow, change: &KindChange) {
    row.delivery = change.delivery;
    row.default_branch.clone_from(&change.default_branch);
    row.github_remote.clone_from(&change.github_remote);
}

/// The project `place` would get, without writing anything: the row at its path (a retired one as
/// it would come back), with any change of kind applied, else a new row's values with a
/// placeholder ID. [`register`] then writes it.
pub(crate) fn preview(
    conn: &Connection,
    place: &Place,
    home: &Path,
    at: Timestamp,
) -> Result<ProjectRow, StoreError> {
    let existing = match projects::active_by_path(conn, &place.root)? {
        Some(row) => Some(row),
        None => projects::retired_by_path(conn, &place.root)?,
    };
    if let Some(mut row) = existing {
        row.removed = false;
        if let Some(change) = kind_change(&row, place) {
            apply(&mut row, &change);
        }
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

/// Records a change of kind, with its event.
fn record_kind_change(
    conn: &Connection,
    row: &ProjectRow,
    change: &KindChange,
    at: Timestamp,
) -> Result<(), StoreError> {
    projects::set_kind(conn, row.id, change)?;
    let what = match (&change.github_remote, change.delivery) {
        (_, DeliveryMode::Folder) => "a plain folder now".to_owned(),
        (Some(remote), _) => format!("a GitHub repository now ({remote})"),
        (None, _) if row.delivery == DeliveryMode::Folder => "a git repository now".to_owned(),
        (None, _) => "a repository with no GitHub remote now".to_owned(),
    };
    let summary = format!(
        "project {} is {what}: new tasks use {}",
        row.name,
        crate::projects::delivery_words(change.delivery)
    );
    events::insert(
        conn,
        &NewEvent {
            kind: event_kind::KIND,
            agent: None,
            task: None,
            turn: None,
            summary: &summary,
            payload: serde_json::json!({
                "project": row.id,
                "name": row.name,
                "from": row.delivery,
                "to": change.delivery,
            }),
            at,
        },
    )
    .map(drop)
}

/// Registers the project for `place`, inside the caller's transaction: the active row at its path,
/// else the retired one brought back with its settings, else a new row. A row whose folder has
/// changed kind since ([`kind_change`]) is updated first.
pub(crate) fn register(
    conn: &Connection,
    place: &Place,
    home: &Path,
    at: Timestamp,
) -> Result<ProjectRow, StoreError> {
    let existing = match projects::active_by_path(conn, &place.root)? {
        Some(row) => Some(row),
        None => projects::retired_by_path(conn, &place.root)?,
    };
    if let Some(row) = &existing
        && let Some(change) = kind_change(row, place)
    {
        record_kind_change(conn, row, &change, at)?;
    }
    if let Some(row) = existing.filter(|r| !r.removed) {
        return projects::load(conn, row.id)?.ok_or_else(|| StoreError::Invalid {
            column: "projects.id",
            message: format!("project {} vanished while it was registered", row.id),
        });
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

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn a_plain_folder_that_becomes_a_repository_changes_kind_for_new_work() {
        let base = scratch("kind");
        let home = base.join("home");
        let notes = home.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let env = env();
        let r = resolver(&home, &data, &env);
        let mut conn = Connection::open_in_memory().unwrap();
        qm_store::schema::migrate(&mut conn).unwrap();
        let at = Timestamp::UNIX_EPOCH;

        let place = r.resolve("~/notes", &[]).unwrap();
        let row = register(&conn, &place, &home, at).unwrap();
        assert_eq!(row.delivery, DeliveryMode::Folder);
        // Listed again: still a plain folder, nothing changes.
        let again = r
            .resolve("~/notes", &plain_folders(&conn).unwrap())
            .unwrap();
        assert!(kind_change(&row, &again).is_none());

        // The user asks for a repository: new work there uses worktrees, in the same project.
        if !git(&notes, &["init", "--quiet", "--initial-branch=main"]) {
            return;
        }
        let place = r
            .resolve("~/notes", &plain_folders(&conn).unwrap())
            .unwrap();
        assert_eq!(
            preview(&conn, &place, &home, at).unwrap().delivery,
            DeliveryMode::BuildInPlace
        );
        let local = register(&conn, &place, &home, at).unwrap();
        assert_eq!(local.id, row.id);
        assert_eq!(local.delivery, DeliveryMode::BuildInPlace);
        assert_eq!(local.default_branch, "main");
        // Named by project, the folder is read again too.
        assert!(kind_change(&local, &r.at_root(local.path.clone())).is_none());

        // Then a GitHub remote: pull requests.
        assert!(git(
            &notes,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/someone/notes.git"
            ]
        ));
        let github = register(&conn, &r.at_root(local.path.clone()), &home, at).unwrap();
        assert_eq!(github.delivery, DeliveryMode::DirectPr);
        assert_eq!(github.github_remote.as_deref(), Some("someone/notes"));

        // A delivery mode set by hand stays while the kind is the same.
        projects::set_kind(
            &conn,
            github.id,
            &KindChange {
                delivery: DeliveryMode::BranchOnly,
                default_branch: "main".into(),
                github_remote: github.github_remote.clone(),
            },
        )
        .unwrap();
        let kept = register(&conn, &r.at_root(github.path.clone()), &home, at).unwrap();
        assert_eq!(kept.delivery, DeliveryMode::BranchOnly);

        // The repository removed: a plain folder again.
        std::fs::remove_dir_all(notes.join(".git")).unwrap();
        let plain = register(&conn, &r.at_root(kept.path.clone()), &home, at).unwrap();
        assert_eq!(plain.delivery, DeliveryMode::Folder);
        assert_eq!(plain.github_remote, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_new_folder_is_made_when_asked_but_never_in_the_app_folder() {
        let base = scratch("create");
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let env = env();
        let r = resolver(&home, &data, &env);
        assert!(r.resolve("~/trips/japan", &[]).is_err());
        r.create("~/trips/japan").unwrap();
        let place = r.resolve("~/trips/japan", &[]).unwrap();
        assert_eq!(place.root, home.join("trips/japan"));
        assert!(place.repo.is_none());
        // Already there: left alone.
        r.create("~/trips/japan").unwrap();
        assert!(r.create(data.join("tasks/x").to_str().unwrap()).is_err());
        assert!(!data.join("tasks").exists());
        assert!(r.create("~/trips/../../data/x").is_err());
        assert!(r.create("trips").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
