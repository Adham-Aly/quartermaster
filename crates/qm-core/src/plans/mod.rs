//! Plan tasks' plans, kept in each task's plan folder (`artifacts/T-n/plan/` in the data
//! directory), never in the database. The retention sweep ([`crate::retention`]) keeps a folder
//! while its task, or a build that reads it, is open.
//!
//! A plan folder holds:
//!
//! | File | Written by | What |
//! |---|---|---|
//! | `plan.md` | the planner | comprehensive technical implementation handoff |
//! | `assets/` | the planner | images it shows |
//! | `plan.html` | the planner, normal autonomy only | human explanation with app-owned review controls |
//! | `template.html` | the app, normal autonomy only | starter fragment and shared styling contract |
//! | `page-source.json` | the app | retained authored fragment and composed-page digest |
//! | `state.json` | the app | [`PlanState`]: the plan's id, stage, round, card and page key |
//!
//! A build that builds on a plan gets its own copy of `plan.md` and `assets/` in its own plan
//! folder, so the plan task can close while the build runs.
//!
//! The user answers a round on its page and sends the reply to the app's loopback endpoint
//! ([`endpoint`], [`submit`]) or copies it into the Quartermaster's chat. Either way one reply per
//! round reaches the planner, as a message from its supervisor.
//!
//! The plan mode a planner's session runs in is the app's choice, from the project's autonomy when
//! the session starts ([`PlanState::session_mode`]); nothing the planner says changes it.

pub mod api;
pub mod endpoint;
pub mod submit;

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
    /// The app note that once carried the plan's card in the chat. No longer set: kept so state
    /// files written before still load.
    #[serde(default)]
    pub card: Option<MessageId>,
    /// Why the page takes no more replies, when it doesn't.
    #[serde(default)]
    pub locked: Option<String>,
    /// The current round's page key, a new one each round: what lets the page send its reply.
    #[serde(default)]
    pub page_token: Option<PageToken>,
    /// A reply sent from the page for this round is being delivered.
    #[serde(default)]
    pub page_claim: Option<u32>,
    /// The round whose reply the page sent, and when.
    #[serde(default)]
    pub page_sent: Option<PageSent>,
    /// What the page on disk was rendered from ([`qm_plans::digest`]): the app renders it again
    /// (for a new port, or once its reply is sent) only while both documents and assets still match,
    /// so the user never sees plan text appear that the planner didn't report.
    #[serde(default)]
    pub rendered_digest: Option<String>,
    /// The endpoint port the page on disk sends to; `None` when it can't send.
    #[serde(default)]
    pub page_port: Option<u16>,
    /// The fingerprints the page on disk was badged against, so rendering it again keeps its
    /// "updated" badges.
    #[serde(default)]
    pub page_previous: BTreeMap<String, String>,
}

/// A round's page key: 32 random bytes from the system's generator, in hex. It lives only in
/// `state.json` (0600) and the page itself; `Debug` leaves it out, and it is never logged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageToken(String);

impl PageToken {
    /// A fresh key; `None` if the system's generator fails (the page then offers Copy reply only).
    pub fn fresh() -> Option<Self> {
        use rand::TryRngCore as _;
        let mut bytes = [0_u8; 32];
        rand::rngs::OsRng.try_fill_bytes(&mut bytes).ok()?;
        let mut hex = String::with_capacity(64);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(hex, "{b:02x}");
        }
        Some(Self(hex))
    }

    /// Whether `given` is this key, compared in time that doesn't depend on where they differ.
    pub fn matches(&self, given: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), given.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    /// The key itself, for the page.
    fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PageToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PageToken(…)")
    }
}

/// The page's reply to a round reached the planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSent {
    pub round: u32,
    /// When, as shown ("14:02").
    pub at: String,
}

impl PlanState {
    /// Whether the page for the current round may send a reply now: the round is with the user,
    /// the page is open, and no reply for it has been sent, from the page or pasted.
    pub fn takes_page_reply(&self) -> bool {
        matches!(self.stage, Stage::WithUser | Stage::Final)
            && self.locked.is_none()
            && !self.pending_reply
            && self.page_claim.is_none()
            && self
                .page_sent
                .as_ref()
                .is_none_or(|s| s.round != self.round)
            && self.last_answered_round < self.round
    }

    /// Whether the current round's reply was sent from the page.
    pub fn sent_from_page(&self) -> bool {
        self.page_sent
            .as_ref()
            .is_some_and(|s| s.round == self.round)
    }
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
            page_token: None,
            page_claim: None,
            page_sent: None,
            rendered_digest: None,
            page_port: None,
            page_previous: BTreeMap::new(),
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

/// Records the user's reply to `round` in `state`, the same for a pasted reply and one sent from
/// the page: the round counts as answered once, and the planner is now working on it.
pub fn record_answer(state: &mut PlanState, round: u32) {
    if state.last_answered_round < round {
        state.last_answered_round = round;
        state.user_rounds += 1;
    }
    state.pending_reply = true;
}

/// Changes the plan's state under the write lock when `change` says yes, and saves it only then;
/// `None` when the folder has none.
pub fn try_update<T, E>(
    dir: &Path,
    change: impl FnOnce(&mut PlanState) -> Result<T, E>,
) -> io::Result<Option<Result<(PlanState, T), E>>> {
    let _guard = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut state) = load(dir)? else {
        return Ok(None);
    };
    match change(&mut state) {
        Ok(out) => {
            save(dir, &state)?;
            Ok(Some(Ok((state, out))))
        }
        Err(e) => Ok(Some(Err(e))),
    }
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
    match mode {
        PlanMode::Internal => {
            if fs::symlink_metadata(dir.join(PAGE_FILE)).is_ok() {
                return Err("Auto autonomy has no plan.html: remove it and keep the technical plan in plan.md".to_owned());
            }
            qm_plans::check(&md, &assets, &rules).map_err(|p| problems(&p))
        }
        PlanMode::Interactive => {
            qm_plans::check_technical(&md, &assets).map_err(|p| problems(&p))?;
            let source = authored_source(dir)?;
            qm_plans::check_html(&source, &assets, &rules).map_err(|p| p.to_string())
        }
    }
}

/// A retained author fragment paired with the exact composed output's fingerprint. A planner
/// revises by replacing plan.html with a new fragment; untouched runtime output resolves back
/// to its source. Runtime refreshes never turn the technical Markdown into user-facing prose.
#[derive(Serialize, Deserialize)]
struct AuthoredPage {
    source: String,
    output: String,
}

const AUTHOR_FILE: &str = "page-source.json";

fn plain_text(dir: &Path, name: &str, max: u64) -> Result<String, String> {
    let path = dir.join(name);
    let meta = fs::symlink_metadata(&path).map_err(|e| format!("{name} can't be read: {e}"))?;
    if !meta.is_file() || meta.len() > max {
        return Err(format!(
            "{name} must be a plain file no larger than {max} bytes"
        ));
    }
    fs::read_to_string(path).map_err(|e| format!("{name} can't be read: {e}"))
}

fn authored_source(dir: &Path) -> Result<String, String> {
    // Composed pages include bundled fonts and scripts, so their cap exceeds the source cap.
    let html = plain_text(dir, PAGE_FILE, 80 * 1024 * 1024)?;
    if dir.join(AUTHOR_FILE).exists() {
        let saved = plain_text(dir, AUTHOR_FILE, 8 * qm_plans::MAX_PLAN_BYTES as u64)?;
        let saved: AuthoredPage = serde_json::from_str(&saved).map_err(|e| e.to_string())?;
        if saved.output == qm_plans::digest(&html, &Assets::default()) {
            return Ok(saved.source);
        }
    }
    Ok(html)
}

fn page_digest(md: &str, source: &str, assets: &Assets) -> String {
    qm_plans::digest(
        &format!(
            "{}{}",
            qm_plans::digest(md, assets),
            qm_plans::digest(source, assets)
        ),
        &Assets::default(),
    )
}

/// Supply only the starter fragment in normal autonomy; never manufacture a plan's prose.
pub fn prepare_template(dir: &Path, mode: PlanMode) -> io::Result<()> {
    let template = dir.join("template.html");
    if mode == PlanMode::Interactive {
        write_private(&template, qm_plans::authoring_template().as_bytes())
    } else {
        for name in ["template.html", PAGE_FILE, AUTHOR_FILE] {
            match fs::remove_file(dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// A page written to the plan folder.
#[derive(Clone, Debug)]
pub struct Rendered {
    pub checked: Checked,
    /// What it was rendered from ([`qm_plans::digest`]).
    pub digest: String,
    /// The endpoint port it sends to, if it can send.
    pub port: Option<u16>,
    /// The fingerprints its "updated" badges compare against.
    pub previous: BTreeMap<String, String>,
}

impl Rendered {
    /// Notes in `state` what the page on disk shows, for rendering it again later.
    pub fn record(&self, state: &mut PlanState) {
        state.rendered_digest = Some(self.digest.clone());
        state.page_port = self.port;
        state.page_previous.clone_from(&self.previous);
    }
}

/// Wraps the planner-authored HTML with review controls for `state`, and writes
/// `plan.html`. With the endpoint's `port`, a page that takes a reply ([`PlanState::takes_page_reply`])
/// can send it there with the round's key; without one it offers Copy reply only.
pub fn render(
    dir: &Path,
    task: TaskId,
    project: &str,
    state: &PlanState,
    previous: BTreeMap<String, String>,
    port: Option<u16>,
) -> Result<Rendered, String> {
    let (md, assets) = read(dir)?;
    render_from(dir, &md, &assets, task, project, state, previous, port)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the page's inputs, passed through from render and rerender"
)]
fn render_from(
    dir: &Path,
    md: &str,
    assets: &Assets,
    task: TaskId,
    project: &str,
    state: &PlanState,
    previous: BTreeMap<String, String>,
    port: Option<u16>,
) -> Result<Rendered, String> {
    let page_state = match (&state.locked, state.stage, &state.approved_at) {
        (Some(reason), _, _) => qm_plans::PageState::Locked {
            reason: reason.clone(),
        },
        (None, Stage::Approved, at) => qm_plans::PageState::Approved {
            at: at.clone().unwrap_or_default(),
        },
        (None, _, _) if state.sent_from_page() => qm_plans::PageState::Locked {
            reason: format!(
                "Your reply to round {} went to the planner at {}. The next round will appear here.",
                state.round,
                state.page_sent.as_ref().map_or("", |s| s.at.as_str())
            ),
        },
        (None, Stage::Final, _) => qm_plans::PageState::Final,
        (None, Stage::Drafting | Stage::WithUser, _) => qm_plans::PageState::Open,
    };
    let submit = match (port, &state.page_token) {
        (Some(port), Some(token)) if state.takes_page_reply() => Some(qm_plans::Submit {
            port,
            token: token.reveal().to_owned(),
        }),
        _ => None,
    };
    let sends_to = submit.as_ref().map(|s| s.port);
    let meta = qm_plans::Meta {
        task: task.to_string(),
        // These inputs have no friendly task name. Use the authored page title for display,
        // on initial renders and refreshes alike; never substitute it for protocol identity.
        task_label: None,
        plan_uid: state.uid.clone(),
        round: state.round.max(1),
        project: project.to_owned(),
        state: page_state,
        previous: previous.clone(),
        submit,
    };
    let source = authored_source(dir)?;
    let rendered = qm_plans::compose_html(&source, assets, &meta).map_err(|p| p.to_string())?;
    let saved = AuthoredPage {
        source: source.clone(),
        output: qm_plans::digest(&rendered.html, &Assets::default()),
    };
    let saved = serde_json::to_vec(&saved).map_err(|e| e.to_string())?;
    write_private(&dir.join(AUTHOR_FILE), &saved).map_err(|e| e.to_string())?;
    write_private(&dir.join(PAGE_FILE), rendered.html.as_bytes())
        .map_err(|e| format!("the page couldn't be written: {e}"))?;
    Ok(Rendered {
        checked: rendered.checked,
        digest: page_digest(md, &source, assets),
        port: sends_to,
        previous,
    })
}

/// Renders the page for the saved state again (after its stage changed), badged against
/// `previous`, for the endpoint on `port`, and records it. `Ok(false)` when the folder has no
/// state or no round's page yet.
pub fn render_saved(
    dir: &Path,
    task: TaskId,
    project: &str,
    port: Option<u16>,
    previous: impl FnOnce(&PlanState) -> BTreeMap<String, String>,
) -> Result<bool, String> {
    render_again(dir, task, project, port, Some(previous))
}

/// Renders the page of the round in the saved state again, for the endpoint on `port` or once
/// its reply was sent, keeping its badges; only while `plan.md` and its assets are what the page
/// on disk was rendered from. `Ok(false)` when it was left alone.
pub fn rerender(
    dir: &Path,
    task: TaskId,
    project: &str,
    port: Option<u16>,
) -> Result<bool, String> {
    render_again(
        dir,
        task,
        project,
        port,
        None::<fn(&PlanState) -> BTreeMap<String, String>>,
    )
}

/// `render_saved` with `previous`, else `rerender`.
fn render_again(
    dir: &Path,
    task: TaskId,
    project: &str,
    port: Option<u16>,
    previous: Option<impl FnOnce(&PlanState) -> BTreeMap<String, String>>,
) -> Result<bool, String> {
    let _guard = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut state) = load(dir).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    if state.round == 0 || !dir.join(PAGE_FILE).is_file() {
        return Ok(false);
    }
    let (md, assets) = read(dir)?;
    let source = authored_source(dir)?;
    if state.rendered_digest.as_deref() != Some(page_digest(&md, &source, &assets).as_str()) {
        return Ok(false);
    }
    let previous =
        previous.map_or_else(|| state.page_previous.clone(), |previous| previous(&state));
    let rendered = render_from(dir, &md, &assets, task, project, &state, previous, port)?;
    rendered.record(&mut state);
    save(dir, &state).map_err(|e| e.to_string())?;
    Ok(true)
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
