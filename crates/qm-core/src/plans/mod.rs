//! Plan tasks' plans, kept only in the scratch root (`/tmp/quartermaster/…/plans/T-n/`), never in
//! the database: plans last hours or days, not weeks, and the user wants nothing of them outside
//! `/tmp`.
//!
//! A plan folder holds:
//!
//! | File | Written by | What |
//! |---|---|---|
//! | `plan.md` | the planner | the plan, the one source of truth |
//! | `assets/` | the planner | images it shows |
//! | `plan.html` | the app, normal autonomy only | the user's page, rendered by `qm-plans` |
//! | `state.json` | the app | [`PlanState`]: the plan's id, stage, round and card |
//!
//! A build that builds on a plan gets its own copy of `plan.md` and `assets/` in its own plan
//! folder, so the plan task can close (and its folder go) while the build runs. Every plan folder
//! goes when its task closes; a startup sweep removes any a crash left behind.
//!
//! The plan mode a planner's session runs in is the app's choice, from the project's autonomy when
//! the session starts ([`PlanState::session_mode`]); nothing the planner says changes it.

pub mod api;

use qm_plans::{ASSETS_DIR, Assets, Checked, PAGE_FILE, PLAN_FILE, Problems, Rules};
use qm_types::data_dir::private_folder;
use qm_types::domain::PlanMode;
use qm_types::ids::{MessageId, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::Path;
use std::sync::Mutex;

/// The app's record of a plan, in its folder.
pub const STATE_FILE: &str = "state.json";

/// Where a plan stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The planner is writing the next round.
    Drafting,
    /// A round's questions are with the user.
    WithUser,
    /// No questions open; waiting for the user's approval (normal autonomy).
    Final,
    /// Approved by the user, or finished under auto autonomy: a build may start on it.
    Approved,
}

/// The app's record of one plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanState {
    /// The plan's random id, 24 hex digits: the reply's key, so a reused task number can't take an
    /// old plan's answers.
    pub uid: String,
    pub stage: Stage,
    /// The last round reported, from 1; 0 before the first.
    pub round: u32,
    /// The question ids open in the last round.
    #[serde(default)]
    pub open: Vec<String>,
    /// Rounds whose answers the user sent.
    #[serde(default)]
    pub user_rounds: u32,
    /// The last round the user sent a reply to.
    #[serde(default)]
    pub last_answered_round: u32,
    /// The user's reply (or approval with the recommendations) went to the planner, which hasn't
    /// reported since: the plan on the page is out of date.
    #[serde(default)]
    pub pending_reply: bool,
    /// The last round's section fingerprints, for the next page's "updated" badges.
    #[serde(default)]
    pub fingerprints: BTreeMap<String, String>,
    /// The user approved and asked the planner to take its recommendations.
    #[serde(default)]
    pub take_recommended: bool,
    /// When the user approved, as shown ("14:30").
    #[serde(default)]
    pub approved_at: Option<String>,
    /// The plan mode the planner's running session was started in.
    #[serde(default)]
    pub session_mode: Option<PlanMode>,
    /// The app note in the Quartermaster's chat that carries the plan's card.
    #[serde(default)]
    pub card: Option<MessageId>,
    /// Why the page takes no more replies, when it doesn't.
    #[serde(default)]
    pub locked: Option<String>,
}

impl PlanState {
    fn new() -> Self {
        let bytes: [u8; 12] = rand::random();
        let mut uid = String::with_capacity(24);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(uid, "{b:02x}");
        }
        Self {
            uid,
            stage: Stage::Drafting,
            round: 0,
            open: Vec::new(),
            user_rounds: 0,
            last_answered_round: 0,
            pending_reply: false,
            fingerprints: BTreeMap::new(),
            take_recommended: false,
            approved_at: None,
            session_mode: None,
            card: None,
            locked: None,
        }
    }
}

/// Serialises every read-modify-write of a `state.json`, so two tools can't lose each other's
/// change.
static WRITING: Mutex<()> = Mutex::new(());

/// Makes a plan task's folder (0700) with an empty `assets/` and a fresh [`PlanState`], unless it
/// already has one.
pub fn create(dir: &Path) -> io::Result<PlanState> {
    private_folder(dir)?;
    private_folder(&dir.join(ASSETS_DIR))?;
    let _guard = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = load(dir)? {
        return Ok(state);
    }
    let state = PlanState::new();
    save(dir, &state)?;
    Ok(state)
}

/// The plan's state, if its folder has one.
pub fn load(dir: &Path) -> io::Result<Option<PlanState>> {
    let path = dir.join(STATE_FILE);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} isn't a plain file",
                path.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let text = fs::read_to_string(&path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| io::Error::other(format!("{} can't be read: {e}", path.display())))
}

/// Changes the plan's state under the write lock; `None` when the folder has none.
pub fn update<T>(
    dir: &Path,
    change: impl FnOnce(&mut PlanState) -> T,
) -> io::Result<Option<(PlanState, T)>> {
    let _guard = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut state) = load(dir)? else {
        return Ok(None);
    };
    let out = change(&mut state);
    save(dir, &state)?;
    Ok(Some((state, out)))
}

fn save(dir: &Path, state: &PlanState) -> io::Result<()> {
    let text = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
    write_private(&dir.join(STATE_FILE), text.as_bytes())
}

/// Writes `bytes` to `path` atomically and durably, 0600.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    qm_types::fsio::write_atomic(path, bytes)
}

/// `plan.md` and its assets, read from the folder; problems name what's wrong.
pub fn read(dir: &Path) -> Result<(String, Assets), String> {
    let path = dir.join(PLAN_FILE);
    let md = match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {
            if meta.len() > qm_plans::MAX_PLAN_BYTES as u64 {
                return Err(format!(
                    "plan.md is over {} KB; split the detail into fewer words or images",
                    qm_plans::MAX_PLAN_BYTES / 1024
                ));
            }
            fs::read_to_string(&path).map_err(|e| format!("plan.md can't be read: {e}"))?
        }
        Ok(_) => return Err("plan.md must be a plain file, not a link or a folder".to_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "there is no plan.md yet: write it at {}",
                path.display()
            ));
        }
        Err(e) => return Err(format!("plan.md can't be read: {e}")),
    };
    let assets = Assets::load(dir).map_err(|p| problems(&p))?;
    Ok((md, assets))
}

/// Every problem, one per line.
pub(crate) fn problems(p: &Problems) -> String {
    format!("plan.md has problems; fix them and try again:\n{p}")
}

/// Checks the folder's plan for the next round under `mode`.
pub fn check(dir: &Path, mode: PlanMode, state: &PlanState) -> Result<Checked, String> {
    let (md, assets) = read(dir)?;
    let rules = Rules {
        mode,
        round: state.round + 1,
        user_rounds: state.user_rounds,
        take_recommended: state.take_recommended,
    };
    qm_plans::check(&md, &assets, &rules).map_err(|p| problems(&p))
}

/// Renders the folder's plan as the user's page, for the round and stage in `state`, and writes
/// `plan.html`.
pub fn render(
    dir: &Path,
    task: TaskId,
    project: &str,
    state: &PlanState,
    previous: BTreeMap<String, String>,
) -> Result<Checked, String> {
    let (md, assets) = read(dir)?;
    let page_state = match (&state.locked, state.stage, &state.approved_at) {
        (Some(reason), _, _) => qm_plans::PageState::Locked {
            reason: reason.clone(),
        },
        (None, Stage::Approved, at) => qm_plans::PageState::Approved {
            at: at.clone().unwrap_or_default(),
        },
        (None, Stage::Final, _) => qm_plans::PageState::Final,
        (None, Stage::Drafting | Stage::WithUser, _) => qm_plans::PageState::Open,
    };
    let meta = qm_plans::Meta {
        task: task.to_string(),
        plan_uid: state.uid.clone(),
        round: state.round.max(1),
        project: project.to_owned(),
        state: page_state,
        previous,
    };
    let rendered = qm_plans::render(&md, &assets, &meta).map_err(|p| problems(&p))?;
    write_private(&dir.join(PAGE_FILE), rendered.html.as_bytes())
        .map_err(|e| format!("the page couldn't be written: {e}"))?;
    Ok(rendered.checked)
}

/// Copies a plan's `plan.md` and `assets/` into a build's own plan folder, replacing what was
/// there. Links are skipped. The copy is made whole in a staging folder beside `to` and then
/// renamed into place, so `to/plan.md` exists only once the whole copy does: until then the
/// retention sweep keeps the plan it is copied from.
pub fn copy_for_build(from: &Path, to: &Path) -> io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = to
        .parent()
        .ok_or_else(|| io::Error::other("a plan folder needs a parent folder"))?;
    private_folder(parent)?;
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = parent.join(format!(
        "{}{}-{n}",
        crate::retention::PLAN_STAGING,
        std::process::id()
    ));
    let made = (|| -> io::Result<()> {
        fs::DirBuilder::new().mode(0o700).create(&staging)?;
        let assets_to = staging.join(ASSETS_DIR);
        fs::DirBuilder::new().mode(0o700).create(&assets_to)?;
        let assets_from = from.join(ASSETS_DIR);
        match fs::read_dir(&assets_from) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        let bytes = fs::read(entry.path())?;
                        write_private(&assets_to.join(entry.file_name()), &bytes)?;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let md = fs::read(from.join(PLAN_FILE))?;
        write_private(&staging.join(PLAN_FILE), &md)?;
        qm_types::data_dir::remove_path(to)?;
        fs::rename(&staging, to)?;
        qm_types::fsio::fsync_dir(parent)
    })();
    if made.is_err() {
        let _ = qm_types::data_dir::remove_path(&staging);
    }
    made
}

#[cfg(test)]
mod tests;
