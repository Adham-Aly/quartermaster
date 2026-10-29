//! The app's own updates: is there a newer release, and installing it when you ask.
//!
//! [`Updates`] asks GitHub for the newest release at launch and then every [`CHECK_EVERY`]
//! (every [`RETRY_EVERY`] after a check that got no answer), in the background: a check never
//! blocks the UI, and a failed one (offline, GitHub down, the rate limit) is only logged. When
//! the newest release is newer than this copy, the snapshot's [`UpdateView`] carries a short
//! notice for the top bar, a badge for the menu's update entry and an "Update to x.y.z" button
//! for its popup; otherwise it carries no notice and no badge, so nothing nags.
//!
//! The update runs only when you press that button ([`Command::StartUpdate`]): the
//! [`ShellInstaller`] downloads that exact release's `quartermaster-installer.sh` (the installer
//! the README tells you to run) and runs it with `/bin/sh`, aimed at the folder this copy runs
//! from. The installer swaps the binary with a rename, so this running copy is untouched; the
//! new version runs from the next launch. A copy that doesn't run from a `bin` folder (a
//! development build in `target/`) isn't updated in place; the popup says how to update it.
//!
//! Wall time decides when a check is due, so a Mac that slept through the interval checks soon
//! after it wakes (the monotonic clock stops while it sleeps).

use crate::api::{ApiPart, Handled};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_github::RepoRef;
use qm_types::BoxFuture;
use qm_types::api::{Command, Snapshot, Styled, Tone, UpdateButton, UpdatePhase, UpdateView};
use qm_types::clock::Clock;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How often the app looks for a newer release.
pub const CHECK_EVERY: Duration = Duration::from_hours(1);
/// How soon it looks again after a check that got no answer.
pub const RETRY_EVERY: Duration = Duration::from_mins(30);
/// How often the background loop wakes to see whether a check is due.
const TICK: Duration = Duration::from_mins(5);
/// How long downloading the installer, and then running it, may each take.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(2);
pub const INSTALL_TIMEOUT: Duration = Duration::from_mins(10);
/// The installer each release carries.
pub const INSTALLER_ASSET: &str = "quartermaster-installer.sh";
/// The installer's last lines kept after a failure.
const TAIL_LINES: usize = 4;

/// A release version: `major.minor.patch`. A pre-release or build suffix is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// "0.2.0", "v0.2.0" or "0.2.0-rc.1"; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let core = s.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let version = Self {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
        };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Its tag: "v0.2.0".
    pub tag: String,
    /// Its page on GitHub.
    pub page: String,
}

/// Where the newest release comes from.
pub trait ReleaseSource: Send + Sync + fmt::Debug {
    /// The newest published release, or why it couldn't be read, in a plain sentence.
    fn latest(&self) -> BoxFuture<'_, Result<Release, String>>;
}

/// The newest release on GitHub, asked without a login (`gh` isn't needed).
#[derive(Clone, Debug)]
pub struct GithubReleases {
    repo: RepoRef,
}

impl GithubReleases {
    pub fn new(repo: RepoRef) -> Self {
        Self { repo }
    }
}

impl ReleaseSource for GithubReleases {
    fn latest(&self) -> BoxFuture<'_, Result<Release, String>> {
        Box::pin(async move {
            let release = qm_github::latest_release(&self.repo)
                .await
                .map_err(|e| e.plain())?;
            let version = Version::parse(&release.tag).ok_or_else(|| {
                format!("the newest release's tag ({}) isn't a version", release.tag)
            })?;
            Ok(Release {
                version,
                tag: release.tag,
                page: release.page,
            })
        })
    }
}

/// Why an install didn't finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallFailure {
    /// One plain sentence: "the installer exited with code 1".
    pub reason: String,
    /// The installer's last lines of output.
    pub tail: Vec<String>,
}

impl InstallFailure {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            tail: Vec::new(),
        }
    }
}

/// What installs a release over this copy.
pub trait Installer: Send + Sync + fmt::Debug {
    /// Why this copy can't be updated in place, in a plain sentence; `None` when it can.
    fn blocked(&self) -> Option<String>;

    /// Installs `release` over this copy.
    fn install<'a>(&'a self, release: &'a Release) -> BoxFuture<'a, Result<(), InstallFailure>>;
}

/// The environment variables the installer gets from the app's own; everything else is left out.
const PASSED_TO_INSTALLER: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
];

/// Folders the installer's own commands are always looked for in, after the app's `PATH`.
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Runs the release's own shell installer, aimed at the folder this copy runs from.
#[derive(Clone)]
pub struct ShellInstaller {
    exe: PathBuf,
    repo: RepoRef,
    env: BTreeMap<String, String>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for ShellInstaller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellInstaller")
            .field("exe", &self.exe)
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

impl ShellInstaller {
    /// `exe` is this running copy; `env` the app's environment, of which only
    /// [`PASSED_TO_INSTALLER`] reaches the installer.
    pub fn new(
        exe: PathBuf,
        repo: RepoRef,
        env: &BTreeMap<String, String>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let mut passed: BTreeMap<String, String> = env
            .iter()
            .filter(|(k, _)| PASSED_TO_INSTALLER.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let path = match passed.get("PATH") {
            Some(path) if !path.is_empty() => format!("{path}:{SYSTEM_PATH}"),
            _ => SYSTEM_PATH.to_owned(),
        };
        passed.insert("PATH".to_owned(), path);
        Self {
            exe,
            repo,
            env: passed,
            clock,
        }
    }

    /// The installer's install folder for this copy: the folder above its `bin` (the installer
    /// puts the binary in `<that>/bin`).
    pub fn prefix(exe: &Path) -> Result<PathBuf, String> {
        let dir = exe.parent().unwrap_or(Path::new("/"));
        match (dir.file_name(), dir.parent()) {
            (Some(name), Some(prefix)) if name == "bin" => Ok(prefix.to_path_buf()),
            _ => Err(format!(
                "This copy runs from {}, which the installer doesn't manage: update it the way \
                 you built it, or install with the command in the README.",
                dir.display()
            )),
        }
    }

    /// The URL of `release`'s installer.
    pub fn installer_url(&self, release: &Release) -> String {
        format!(
            "https://github.com/{}/{}/releases/download/{}/{INSTALLER_ASSET}",
            self.repo.owner, self.repo.name, release.tag
        )
    }

    async fn run(
        &self,
        program: &str,
        args: &[&std::ffi::OsStr],
        extra_env: &[(&str, &std::ffi::OsStr)],
        limit: Duration,
    ) -> Result<std::process::Output, InstallFailure> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command
            .spawn()
            .map_err(|e| InstallFailure::new(format!("couldn't start {program}: {e}")))?;
        tokio::select! {
            output = child.wait_with_output() => output
                .map_err(|e| InstallFailure::new(format!("couldn't read {program}'s output: {e}"))),
            () = self.clock.sleep(limit) => Err(InstallFailure::new(format!(
                "{program} didn't finish within {} minutes",
                limit.as_secs() / 60
            ))),
        }
    }

    async fn install_in(&self, dir: &Path, release: &Release) -> Result<(), InstallFailure> {
        let prefix = Self::prefix(&self.exe).map_err(InstallFailure::new)?;
        let script = dir.join(INSTALLER_ASSET);
        let url = self.installer_url(release);
        let download = self
            .run(
                "/usr/bin/curl",
                &[
                    "--proto".as_ref(),
                    "=https".as_ref(),
                    "--tlsv1.2".as_ref(),
                    "-fsSL".as_ref(),
                    "-o".as_ref(),
                    script.as_os_str(),
                    url.as_ref(),
                ],
                &[],
                DOWNLOAD_TIMEOUT,
            )
            .await?;
        if !download.status.success() {
            return Err(InstallFailure {
                reason: format!("couldn't download {url}"),
                tail: tail(&download),
            });
        }
        let before = inode(&self.exe);
        let installed = self
            .run(
                "/bin/sh",
                &[script.as_os_str()],
                &[
                    ("QUARTERMASTER_INSTALL_DIR", prefix.as_os_str()),
                    // The first install set up your PATH; an update leaves your shell's files
                    // alone.
                    ("QUARTERMASTER_NO_MODIFY_PATH", "1".as_ref()),
                ],
                INSTALL_TIMEOUT,
            )
            .await?;
        if !installed.status.success() {
            let reason = installed.status.code().map_or_else(
                || "the installer was stopped".to_owned(),
                |code| format!("the installer exited with code {code}"),
            );
            return Err(InstallFailure {
                reason,
                tail: tail(&installed),
            });
        }
        // The installer moves the new binary into place, so the file there is a new one.
        if inode(&self.exe).is_none() || inode(&self.exe) == before {
            return Err(InstallFailure {
                reason: format!(
                    "the installer finished but didn't replace {}",
                    self.exe.display()
                ),
                tail: tail(&installed),
            });
        }
        Ok(())
    }
}

impl Installer for ShellInstaller {
    fn blocked(&self) -> Option<String> {
        Self::prefix(&self.exe).err()
    }

    fn install<'a>(&'a self, release: &'a Release) -> BoxFuture<'a, Result<(), InstallFailure>> {
        Box::pin(async move {
            let dir = scratch_dir(self.env.get("TMPDIR").map(PathBuf::from)).map_err(|e| {
                InstallFailure::new(format!("couldn't make a folder to work in: {e}"))
            })?;
            let result = self.install_in(&dir, release).await;
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::debug!(
                    dir = %dir.display(),
                    error = %e,
                    "couldn't remove the update's folder"
                );
            }
            result
        })
    }
}

/// A new private folder for one install, in `tmp` (else the system's).
fn scratch_dir(tmp: Option<PathBuf>) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let base = tmp.unwrap_or_else(std::env::temp_dir);
    let mut attempt = 0_u32;
    loop {
        let nanos = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |d| d.subsec_nanos());
        let dir = base.join(format!(
            "quartermaster-update-{}-{nanos}",
            std::process::id()
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 5 => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn inode(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}

/// The last lines of a command's output: its errors if it printed any, else what it printed.
fn tail(output: &std::process::Output) -> Vec<String> {
    let text = if output.stderr.iter().any(|b| !b.is_ascii_whitespace()) {
        &output.stderr
    } else {
        &output.stdout
    };
    let text = String::from_utf8_lossy(text);
    let lines: Vec<String> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].to_vec()
}

/// Where an install stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Install {
    #[default]
    Idle,
    Running(Release),
    Done(Release),
    Failed(Release, InstallFailure),
}

/// What the app knows about updates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    checking: bool,
    /// The newest release the last answered check found.
    latest: Option<Release>,
    /// When a check last answered.
    checked_at: Option<Timestamp>,
    /// When a check last started.
    tried_at: Option<Timestamp>,
    /// Why the last check got no answer; cleared by one that does.
    check_error: Option<String>,
    install: Install,
}

impl State {
    /// The release to install: the newest, when it is newer than `current`.
    fn newer(&self, current: Version) -> Option<&Release> {
        self.latest.as_ref().filter(|r| r.version > current)
    }

    /// A check is due at `now`: none has run, or the last ran an interval ago. Never while one
    /// runs, or while an install runs or waits for a restart.
    fn due(&self, now: Timestamp) -> bool {
        if self.checking || matches!(self.install, Install::Running(_) | Install::Done(_)) {
            return false;
        }
        let Some(tried) = self.tried_at else {
            return true;
        };
        let every = if self.check_error.is_some() {
            RETRY_EVERY
        } else {
            CHECK_EVERY
        };
        now.duration_since(tried).unsigned_abs() >= every || now < tried
    }
}

/// What [`Updates`] runs on.
#[derive(Clone)]
pub struct UpdatesDeps {
    /// This copy's version.
    pub current: Version,
    pub source: Arc<dyn ReleaseSource>,
    pub installer: Arc<dyn Installer>,
    pub clock: Arc<dyn Clock>,
    /// Something the update view shows changed: send the UI a fresh snapshot.
    pub changed: Arc<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for UpdatesDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdatesDeps")
            .field("current", &self.current)
            .field("source", &self.source)
            .field("installer", &self.installer)
            .finish_non_exhaustive()
    }
}

/// The update check and the install. Cheap to clone.
#[derive(Clone)]
pub struct Updates {
    inner: Arc<Inner>,
}

struct Inner {
    deps: UpdatesDeps,
    state: Mutex<State>,
}

impl fmt::Debug for Updates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Updates")
            .field("current", &self.inner.deps.current)
            .field("source", &self.inner.deps.source)
            .field("installer", &self.inner.deps.installer)
            .finish_non_exhaustive()
    }
}

impl Updates {
    pub fn new(deps: UpdatesDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Checks now in the background, then whenever a check is due, for as long as the app runs.
    pub fn start(&self) {
        let updates = self.clone();
        tokio::spawn(async move {
            loop {
                let now = updates.inner.deps.clock.now();
                if updates.state().due(now) {
                    updates.check().await;
                }
                updates.inner.deps.clock.sleep(TICK).await;
            }
        });
    }

    /// Asks for the newest release once, unless a check or an install is running.
    pub async fn check(&self) {
        let deps = &self.inner.deps;
        {
            let mut state = self.state();
            if state.checking || matches!(state.install, Install::Running(_)) {
                return;
            }
            state.checking = true;
            state.tried_at = Some(deps.clock.now());
        }
        (deps.changed)();
        let answer = deps.source.latest().await;
        {
            let mut state = self.state();
            state.checking = false;
            match answer {
                Ok(release) => {
                    tracing::info!(
                        current = %deps.current,
                        latest = %release.version,
                        "checked for an update"
                    );
                    state.checked_at = Some(deps.clock.now());
                    state.check_error = None;
                    let newer = release.version > deps.current;
                    state.latest = Some(release);
                    // You updated some other way since an install failed: nothing to retry.
                    if !newer && matches!(state.install, Install::Failed(..)) {
                        state.install = Install::Idle;
                    }
                }
                Err(e) => {
                    if state.check_error.as_deref() == Some(e.as_str()) {
                        tracing::debug!(error = %e, "couldn't check for an update");
                    } else {
                        tracing::info!(error = %e, "couldn't check for an update");
                    }
                    state.check_error = Some(e);
                }
            }
        }
        (deps.changed)();
    }

    /// Starts installing the newer release in the background. `false` (and nothing starts) when
    /// there is none, this copy can't be updated in place, or an install already ran.
    pub fn start_install(&self) -> bool {
        let deps = &self.inner.deps;
        if let Some(why) = deps.installer.blocked() {
            tracing::info!(reason = %why, "an update was asked for but can't run here");
            return false;
        }
        let release = {
            let mut state = self.state();
            if matches!(state.install, Install::Running(_) | Install::Done(_)) {
                return false;
            }
            let Some(release) = state.newer(deps.current).cloned() else {
                return false;
            };
            state.install = Install::Running(release.clone());
            release
        };
        tracing::info!(from = %deps.current, to = %release.version, "installing an update");
        let updates = self.clone();
        tokio::spawn(async move {
            let result = updates.inner.deps.installer.install(&release).await;
            let install = match result {
                Ok(()) => {
                    tracing::info!(version = %release.version, "update installed");
                    Install::Done(release)
                }
                Err(failure) => {
                    tracing::warn!(
                        version = %release.version,
                        reason = %failure.reason,
                        output = ?failure.tail,
                        "the update didn't finish"
                    );
                    Install::Failed(release, failure)
                }
            };
            updates.state().install = install;
            (updates.inner.deps.changed)();
        });
        (deps.changed)();
        true
    }

    /// What the UI shows.
    pub fn view(&self) -> UpdateView {
        let deps = &self.inner.deps;
        let state = self.state().clone();
        view(
            &state,
            deps.current,
            deps.installer.blocked(),
            &TimeZone::system(),
        )
    }
}

/// The update view for `state`, with `blocked` saying why this copy can't update itself.
fn view(state: &State, current: Version, blocked: Option<String>, tz: &TimeZone) -> UpdateView {
    let mut view = UpdateView::default();
    view.current = current.to_string();
    if !install_view(&mut view, &state.install) {
        check_view(&mut view, state, current, blocked, tz);
    }
    view
}

/// Fills `view` for a running, finished or failed install; `false` when none ran.
fn install_view(view: &mut UpdateView, install: &Install) -> bool {
    let release = match install {
        Install::Idle => return false,
        Install::Running(release) | Install::Done(release) | Install::Failed(release, _) => release,
    };
    let version = release.version;
    view.available = Some(version.to_string());
    view.release_page = Some(release.page.clone());
    match install {
        Install::Running(_) => {
            view.phase = UpdatePhase::Installing;
            view.status = Styled::new(format!("Installing Quartermaster {version}…"), Tone::Text);
            view.notice = Some(Styled::new(format!("updating to {version}…"), Tone::Dim));
        }
        Install::Done(_) => {
            view.phase = UpdatePhase::Installed;
            view.badge = true;
            view.status = Styled::new(
                format!("Quartermaster {version} is installed. Quit and reopen the app to use it."),
                Tone::Ok,
            );
            view.notice = Some(Styled::new(
                format!("{version} installed · reopen to use it"),
                Tone::Ok,
            ));
        }
        Install::Failed(_, failure) => {
            view.phase = UpdatePhase::Failed;
            view.badge = true;
            view.status = Styled::new(
                format!(
                    "The update to {version} didn't finish: {}. This copy is unchanged.",
                    failure.reason
                ),
                Tone::Error,
            );
            view.detail.clone_from(&failure.tail);
            view.notice = Some(Styled::new("update failed", Tone::Warn));
            view.button = Some(UpdateButton::new("Try again", Command::StartUpdate));
        }
        Install::Idle => {}
    }
    true
}

/// Fills `view` from the checks, when no install ran.
fn check_view(
    view: &mut UpdateView,
    state: &State,
    current: Version,
    blocked: Option<String>,
    tz: &TimeZone,
) {
    let newer = state.newer(current);
    view.available = newer.map(|r| r.version.to_string());
    if state.checking {
        view.phase = UpdatePhase::Checking;
        view.status = Styled::new("Checking for a newer version…", Tone::Dim);
        return;
    }
    let check_now = || UpdateButton::new("Check now", Command::CheckForUpdate);
    if let Some(release) = newer {
        let version = release.version;
        view.phase = UpdatePhase::Available;
        view.badge = true;
        view.release_page = Some(release.page.clone());
        view.status = Styled::new(
            format!("Quartermaster {version} is available. You have {current}."),
            Tone::Accent,
        );
        view.notice = Some(Styled::new(
            format!("update {version} available"),
            Tone::Accent,
        ));
        match blocked {
            Some(why) => view.detail.push(why),
            None => {
                view.button = Some(UpdateButton::new(
                    format!("Update to {version}"),
                    Command::StartUpdate,
                ));
            }
        }
    } else if state.latest.is_some() {
        view.phase = UpdatePhase::UpToDate;
        view.status = Styled::new(
            format!("Quartermaster {current} is the latest version."),
            Tone::Ok,
        );
        view.button = Some(check_now());
    } else {
        view.phase = UpdatePhase::Unknown;
        let status = if state.check_error.is_some() {
            format!("You have Quartermaster {current}. The app couldn't check for a newer version.")
        } else {
            format!("You have Quartermaster {current}. It hasn't been checked for updates yet.")
        };
        view.status = Styled::new(status, Tone::Dim);
        view.button = Some(check_now());
    }
    if let Some(error) = &state.check_error {
        view.detail.push(format!(
            "The last check got no answer ({error}); it tries again every {} minutes.",
            RETRY_EVERY.as_secs() / 60
        ));
    }
    if let Some(at) = state.checked_at {
        view.detail.push(format!(
            "Last checked at {}.",
            at.to_zoned(tz.clone()).strftime("%H:%M")
        ));
    }
}

/// The API's end of updates: [`Command::CheckForUpdate`], [`Command::StartUpdate`], and
/// `Snapshot.update`.
#[derive(Debug)]
pub struct UpdatesPart {
    updates: Updates,
}

impl UpdatesPart {
    pub fn new(updates: Updates) -> Self {
        Self { updates }
    }
}

impl ApiPart for UpdatesPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::CheckForUpdate => Some(Box::pin(async move {
                // In the background: a slow network never holds up the UI's next command.
                let updates = self.updates.clone();
                tokio::spawn(async move { updates.check().await });
                Handled::Unchanged
            })),
            Command::StartUpdate => Some(Box::pin(async move {
                if self.updates.start_install() {
                    Handled::Changed
                } else {
                    Handled::Unchanged
                }
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            snapshot.update = self.updates.view();
        })
    }
}
