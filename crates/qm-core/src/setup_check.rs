//! The setup check: what this Mac has of `git`, `gh` and `claude`, and what a missing one switches
//! off.
//!
//! The app always starts. [`detect`] finds each tool on the `PATH` it is given (and `claude` at
//! `~/.local/bin/claude` too), reads its version, and asks `gh auth status` and `claude auth
//! status --json` whether GitHub and a Claude subscription are logged in. It runs nothing else
//! and installs nothing: installs and logins run only from the setup screen's terminal pane,
//! after your click, and never with `sudo` (the commands are in [`view`]).
//!
//! [`SetupCheck`] runs detection off the async threads in the background and publishes the facts
//! on a `watch::Receiver<SetupFacts>` (the delivery probe reads it). The screen never opens by
//! itself: what needs you shows as a red dot on the menu and in Settings (`Snapshot.setup_issues`),
//! and goes away once it's fixed. [`SetupPart`] is the API's end: it re-runs the check on
//! `Command::RecheckSetup` and fills `Snapshot.setup` and `Snapshot.setup_issues`.
//!
//! What each missing tool switches off ([`SetupFacts::switched_off`]): without `gh`, pull
//! requests, branch delivery, GitHub clones and CI results; without a `claude` subscription
//! login, the Claude engine (an API-key login isn't used); without `git`, everything.

use crate::api::{ApiPart, Conversation, Handled};
use crate::turn_io::ChatLog;
use qm_types::BoxFuture;
use qm_types::api::{
    Command, SetupAction, SetupActionKind, SetupItem, SetupMark, SetupStep, SetupTool, SetupView,
    Snapshot, Styled, Tone,
};
use qm_types::clock::Clock;
use qm_types::env::NEVER_PASSED_TO_AGENTS;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long one detection command may run.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the check runs again in the background, so the red dot comes and goes with what's on
/// this Mac. The UI asks for a check sooner when the terminal regains focus while something needs
/// you.
pub const RECHECK_EVERY: Duration = Duration::from_mins(5);

/// Homebrew's official installer, shown to copy when Homebrew is missing.
pub const HOMEBREW_INSTALL: &str = "/bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"";
/// Claude Code's official installer, shown to copy when neither it nor Homebrew is installed.
pub const CLAUDE_INSTALL: &str = "curl -fsSL https://claude.ai/install.sh | bash";

/// One tool's state on this Mac.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolState {
    #[default]
    Missing,
    /// Installed; for `git` that's all there is to know, for `gh` and `claude` the login couldn't
    /// be read.
    Installed {
        version: String,
    },
    LoggedOut {
        version: String,
    },
    /// Logged in: `account` is the GitHub login, or the Claude subscription ("Claude Max
    /// subscription").
    LoggedIn {
        version: String,
        account: String,
    },
    /// `claude` is logged in with an API key, which the app doesn't use.
    ApiKey {
        version: String,
    },
}

impl ToolState {
    /// The installed version, if installed.
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Missing => None,
            Self::Installed { version }
            | Self::LoggedOut { version }
            | Self::LoggedIn { version, .. }
            | Self::ApiKey { version } => Some(version),
        }
    }

    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub fn is_logged_in(&self) -> bool {
        matches!(self, Self::LoggedIn { .. })
    }

    fn word(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Installed { .. } => "installed",
            Self::LoggedOut { .. } => "logged_out",
            Self::LoggedIn { .. } => "logged_in",
            Self::ApiKey { .. } => "api_key",
        }
    }
}

/// Homebrew, when it's on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Homebrew {
    /// "4.6".
    pub version: String,
    /// "/opt/homebrew".
    pub prefix: PathBuf,
    /// The `brew` found.
    pub path: PathBuf,
}

/// Where each tool was found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPaths {
    pub git: Option<PathBuf>,
    pub gh: Option<PathBuf>,
    pub claude: Option<PathBuf>,
}

/// What the check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupFacts {
    /// The check has run since the app started; until then every tool reads `Missing`.
    pub checked: bool,
    pub git: ToolState,
    pub gh: ToolState,
    pub claude: ToolState,
    /// Where `git` comes from: "Xcode Command Line Tools", "Homebrew", or its folder.
    pub git_source: Option<String>,
    /// The scopes of `gh`'s token: "repo", "workflow", "read:org".
    pub gh_scopes: Vec<String>,
    pub homebrew: Option<Homebrew>,
    pub paths: ToolPaths,
}

/// Something a missing tool switches off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// Without `git`, nothing works.
    Everything,
    PullRequests,
    BranchDelivery,
    GithubClones,
    CiResults,
    ClaudeEngine,
}

impl SetupFacts {
    /// `gh` is installed and logged in: pull requests, branch delivery, GitHub clones and CI work.
    pub fn gh_ready(&self) -> bool {
        self.gh.is_logged_in()
    }

    /// `claude` is logged in with a Claude subscription: the Claude engine is available.
    pub fn claude_ready(&self) -> bool {
        self.claude.is_logged_in()
    }

    /// What the missing or logged-out tools switch off.
    pub fn switched_off(&self) -> Vec<Feature> {
        if !self.checked {
            return Vec::new();
        }
        let mut off = Vec::new();
        if self.git.is_missing() {
            off.push(Feature::Everything);
        }
        if !self.gh_ready() {
            off.extend([
                Feature::PullRequests,
                Feature::BranchDelivery,
                Feature::GithubClones,
                Feature::CiResults,
            ]);
        }
        if !self.claude_ready() {
            off.push(Feature::ClaudeEngine);
        }
        off
    }

    /// One short phrase per thing that needs you; any at all shows the red dot.
    pub fn issues(&self) -> Vec<String> {
        if !self.checked {
            return Vec::new();
        }
        let mut issues = Vec::new();
        if self.git.is_missing() {
            issues.push("git not installed".to_owned());
        }
        match &self.claude {
            ToolState::Missing => issues.push("Claude Code not installed".to_owned()),
            ToolState::LoggedOut { .. } => issues.push("Claude Code not logged in".to_owned()),
            ToolState::ApiKey { .. } => {
                issues.push("Claude Code logged in with an API key".to_owned());
            }
            ToolState::Installed { .. } => {
                issues.push("Claude Code's login couldn't be read".to_owned());
            }
            ToolState::LoggedIn { .. } => {}
        }
        match &self.gh {
            ToolState::Missing => issues.push("GitHub CLI (gh) not installed".to_owned()),
            ToolState::LoggedIn { .. } => {}
            _ => issues.push("GitHub CLI (gh) not logged in".to_owned()),
        }
        issues
    }
}

/// Finds `name` in the `PATH` of `env`.
pub fn find_on_path(env: &BTreeMap<String, String>, name: &str) -> Option<PathBuf> {
    env.get("PATH")?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What one command left behind.
struct Ran {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs `program args` with exactly `env`, stdin closed, for at most [`COMMAND_TIMEOUT`]. `None`
/// when it couldn't start or ran out of time.
fn run(program: &Path, args: &[&str], env: &BTreeMap<String, String>) -> Option<Ran> {
    let cwd = env
        .get("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut child = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(env)
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let reader = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut out = String::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_string(&mut out);
            }
            out
        })
    };
    let stdout = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    status.map(|status| Ran {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// The first `x.y.z` (or `x.y`) in a version line: `git version 2.50.1 (Apple Git-155)`,
/// `gh version 2.92.0 (2025-09-01)`, `2.1.303 (Claude Code)`.
pub fn parse_version(output: &str) -> Option<String> {
    output
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|w| !w.is_empty())
        .find_map(|word| {
            let parts: Vec<&str> = word.split('.').filter(|p| !p.is_empty()).collect();
            (parts.len() >= 2).then(|| parts[..parts.len().min(3)].join("."))
        })
}

/// Homebrew's own `bin` folders, where `brew` and everything it installs (`gh`, Claude Code's
/// cask) land: Apple silicon's prefix, then Intel's.
pub const HOMEBREW_BINS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

/// Names Homebrew's `bin` folders instead of [`HOMEBREW_BINS`]: colon-separated, empty for
/// none (a Homebrew in a prefix of its own, or a test that must not see this Mac's Homebrew).
pub const HOMEBREW_BINS_VAR: &str = "QM_HOMEBREW_BINS";

/// Homebrew's `bin` folders for `env`: [`HOMEBREW_BINS_VAR`] when it is set, else
/// [`HOMEBREW_BINS`].
pub fn homebrew_bins(env: &BTreeMap<String, String>) -> Vec<PathBuf> {
    match env.get(HOMEBREW_BINS_VAR) {
        Some(list) => list
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .collect(),
        None => HOMEBREW_BINS.iter().map(PathBuf::from).collect(),
    }
}

/// Detects `git`, `gh`, `claude` and Homebrew with the environment `env` (its `PATH` decides
/// what is found, with [`homebrew_bins`] after it). Blocking: run it off the async threads.
pub fn detect(env: &BTreeMap<String, String>) -> SetupFacts {
    detect_with(env, &homebrew_bins(env))
}

/// As [`detect`], with `homebrew_bins` as Homebrew's `bin` folders. Homebrew installed from the
/// Setup check's pane (or in another terminal) lands in one of them, which the `PATH` the app
/// started with doesn't name until a new terminal runs Homebrew's `shellenv`; so a folder of
/// them holding a `brew` is searched after `PATH`, and a recheck finds `brew`, and then the
/// `gh` it installs, without a restart.
pub fn detect_with(env: &BTreeMap<String, String>, homebrew_bins: &[PathBuf]) -> SetupFacts {
    let env = &with_homebrew_bins(env, homebrew_bins);
    let homebrew = detect_homebrew(env);
    let ((git, git_source, git_path), (gh, gh_scopes, gh_path), (claude, claude_path)) =
        std::thread::scope(|s| {
            let git = s.spawn(|| detect_git(env, homebrew.as_ref()));
            let gh = s.spawn(|| detect_gh(env));
            let claude = s.spawn(|| detect_claude(env));
            (
                git.join().unwrap_or((ToolState::Missing, None, None)),
                gh.join().unwrap_or((ToolState::Missing, Vec::new(), None)),
                claude.join().unwrap_or((ToolState::Missing, None)),
            )
        });
    SetupFacts {
        checked: true,
        git,
        gh,
        claude,
        git_source,
        gh_scopes,
        homebrew,
        paths: ToolPaths {
            git: git_path,
            gh: gh_path,
            claude: claude_path,
        },
    }
}

/// `env` with each of `homebrew_bins` that holds a `brew` added to the end of its `PATH`, unless
/// `PATH` names it already (where it is, it stays: your order wins).
fn with_homebrew_bins(
    env: &BTreeMap<String, String>,
    homebrew_bins: &[PathBuf],
) -> BTreeMap<String, String> {
    let mut env = env.clone();
    let mut dirs: Vec<String> = env
        .get("PATH")
        .map(|p| {
            p.split(':')
                .filter(|d| !d.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    for bin in homebrew_bins {
        let named = dirs.iter().any(|d| Path::new(d) == bin.as_path());
        if !named && is_executable(&bin.join("brew")) {
            dirs.push(bin.display().to_string());
        }
    }
    env.insert("PATH".to_owned(), dirs.join(":"));
    env
}

fn detect_homebrew(env: &BTreeMap<String, String>) -> Option<Homebrew> {
    let path = find_on_path(env, "brew")?;
    let ran = run(&path, &["--version"], env).filter(|r| r.success)?;
    let full = parse_version(&ran.stdout)?;
    let version = full.split('.').take(2).collect::<Vec<_>>().join(".");
    let prefix = path
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("/"), Path::to_owned);
    Some(Homebrew {
        version,
        prefix,
        path,
    })
}

/// `/usr/bin/git` is Apple's stub: it's only real once the Command Line Tools (or Xcode) are
/// installed, and running it before that opens Apple's installer, so ask `xcode-select` instead.
const APPLE_GIT_STUB: &str = "/usr/bin/git";

fn detect_git(
    env: &BTreeMap<String, String>,
    homebrew: Option<&Homebrew>,
) -> (ToolState, Option<String>, Option<PathBuf>) {
    let Some(path) = find_on_path(env, "git") else {
        return (ToolState::Missing, None, None);
    };
    let mut source = None;
    if path == Path::new(APPLE_GIT_STUB) {
        match run(Path::new("/usr/bin/xcode-select"), &["-p"], env) {
            Some(r) if r.success => {
                source = Some(if r.stdout.contains("Xcode.app") {
                    "Xcode".to_owned()
                } else {
                    "Xcode Command Line Tools".to_owned()
                });
            }
            _ => return (ToolState::Missing, None, None),
        }
    } else if homebrew.is_some_and(|b| path.starts_with(&b.prefix)) {
        source = Some("Homebrew".to_owned());
    } else if let Some(dir) = path.parent() {
        source = Some(dir.display().to_string());
    }
    match run(&path, &["--version"], env)
        .filter(|r| r.success)
        .and_then(|r| parse_version(&r.stdout))
    {
        Some(version) => (ToolState::Installed { version }, source, Some(path)),
        None => (ToolState::Missing, None, None),
    }
}

fn detect_gh(env: &BTreeMap<String, String>) -> (ToolState, Vec<String>, Option<PathBuf>) {
    let Some(path) = find_on_path(env, "gh") else {
        return (ToolState::Missing, Vec::new(), None);
    };
    let Some(version) = run(&path, &["--version"], env)
        .filter(|r| r.success)
        .and_then(|r| parse_version(&r.stdout))
    else {
        return (ToolState::Missing, Vec::new(), None);
    };
    let state = match run(&path, &["auth", "status", "--hostname", "github.com"], env) {
        None => (ToolState::Installed { version }, Vec::new()),
        Some(ran) => {
            let text = format!("{}\n{}", ran.stdout, ran.stderr);
            match (ran.success, parse_gh_account(&text)) {
                (true, Some(account)) => (
                    ToolState::LoggedIn { version, account },
                    parse_gh_scopes(&text),
                ),
                (true, None) => (
                    ToolState::LoggedIn {
                        version,
                        account: String::new(),
                    },
                    parse_gh_scopes(&text),
                ),
                (false, _) => (ToolState::LoggedOut { version }, Vec::new()),
            }
        }
    };
    (state.0, state.1, Some(path))
}

/// The login in `gh auth status`: "Logged in to github.com account adham-aly (keyring)" (gh 2.40
/// and later) or "Logged in to github.com as adham-aly (…)".
pub fn parse_gh_account(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.split("Logged in to ").nth(1)?;
        let mut words = rest.split_whitespace().skip(1);
        match words.next()? {
            "account" | "as" => words.next().map(str::to_owned),
            _ => None,
        }
    })
}

/// The token scopes in `gh auth status`: "- Token scopes: 'repo', 'workflow', 'read:org'".
pub fn parse_gh_scopes(text: &str) -> Vec<String> {
    text.lines()
        .find_map(|line| line.split("Token scopes:").nth(1))
        .map(|scopes| {
            scopes
                .split(',')
                .map(|s| s.trim().trim_matches(['\'', '"']).to_owned())
                .filter(|s| !s.is_empty() && s != "none")
                .collect()
        })
        .unwrap_or_default()
}

/// The environment `claude` is asked with: the app's own, without the keys no agent ever gets,
/// so it reports the login an agent would use. `CLAUDE_CONFIG_DIR` is left exactly as the app
/// was started with it, never set.
fn claude_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(k, _)| !NEVER_PASSED_TO_AGENTS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn find_claude(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    find_on_path(env, "claude").or_else(|| {
        let candidate = Path::new(env.get("HOME")?).join(".local/bin/claude");
        is_executable(&candidate).then_some(candidate)
    })
}

fn detect_claude(env: &BTreeMap<String, String>) -> (ToolState, Option<PathBuf>) {
    let env = claude_env(env);
    let Some(path) = find_claude(&env) else {
        return (ToolState::Missing, None);
    };
    let Some(version) = run(&path, &["--version"], &env)
        .filter(|r| r.success)
        .and_then(|r| parse_version(&r.stdout))
    else {
        return (ToolState::Missing, None);
    };
    let state = match run(&path, &["auth", "status", "--json"], &env) {
        Some(ran) => parse_claude_login(&ran.stdout, version),
        None => ToolState::Installed { version },
    };
    (state, Some(path))
}

/// Reads `claude auth status --json`: only `loggedIn: true` with `authMethod: "claude.ai"` and a
/// `subscriptionType` is a subscription; any other login is an API key.
pub fn parse_claude_login(output: &str, version: String) -> ToolState {
    let Ok(status) = serde_json::from_str::<serde_json::Value>(output.trim()) else {
        return ToolState::Installed { version };
    };
    if status.get("loggedIn").and_then(serde_json::Value::as_bool) != Some(true) {
        return ToolState::LoggedOut { version };
    }
    let method = status.get("authMethod").and_then(serde_json::Value::as_str);
    let subscription = status
        .get("subscriptionType")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    match (method, subscription) {
        (Some("claude.ai"), Some(subscription)) => ToolState::LoggedIn {
            version,
            account: format!("Claude {} subscription", plan_name(subscription)),
        },
        (Some("claude.ai"), None) => ToolState::Installed { version },
        _ => ToolState::ApiKey { version },
    }
}

/// "max" → "Max".
fn plan_name(subscription: &str) -> String {
    let mut chars = subscription.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn dim(text: impl Into<String>) -> Styled {
    Styled::new(text, Tone::Dim)
}

fn run_action(label: &str, title: &str, argv: Vec<String>, verb: &str) -> SetupAction {
    SetupAction::new(
        label,
        SetupActionKind::Run {
            title: title.to_owned(),
            argv,
            verb: verb.to_owned(),
        },
    )
}

fn copy_action(text: &str) -> SetupAction {
    SetupAction::new(
        "Copy the command",
        SetupActionKind::Copy {
            text: text.to_owned(),
        },
    )
}

fn later() -> SetupAction {
    SetupAction::new("Not now", SetupActionKind::Later)
}

/// The fix line Settings ▸ System shows: "Log in to GitHub: press Open, or run gh auth login".
fn fix(what: &str, command: &str) -> String {
    format!("{what}: press Open, or run {command}.")
}

fn program(path: Option<&PathBuf>, name: &str) -> String {
    path.map_or_else(|| name.to_owned(), |p| p.display().to_string())
}

fn git_item(facts: &SetupFacts) -> SetupItem {
    match &facts.git {
        ToolState::Missing => {
            let mut item = SetupItem::new(SetupTool::Git, SetupMark::Missing, "git");
            item.state = vec![Styled::new("not installed", Tone::Error)];
            "git · not installed".clone_into(&mut item.summary);
            item.explanation = Some(
                "Needed for everything: projects, worktrees and delivery all go through git. \
                 Installing Apple's Command Line Tools opens Apple's own installer."
                    .to_owned(),
            );
            item.actions = vec![
                run_action(
                    "Install the Command Line Tools",
                    "xcode-select --install",
                    vec!["/usr/bin/xcode-select".to_owned(), "--install".to_owned()],
                    "install",
                ),
                copy_action("xcode-select --install"),
            ];
            item.fix = Some(fix(
                "Install Apple's Command Line Tools",
                "xcode-select --install",
            ));
            item
        }
        state => {
            let version = state.version().unwrap_or_default();
            let mut item = SetupItem::new(SetupTool::Git, SetupMark::Ok, format!("git {version}"));
            if let Some(source) = &facts.git_source {
                item.state = vec![dim(source.clone())];
            }
            item.summary = format!("git {version}");
            item
        }
    }
}

/// Claude Code isn't installed: Homebrew installs it, else the official installer's command.
fn claude_missing_item(facts: &SetupFacts) -> SetupItem {
    let mut item = SetupItem::new(SetupTool::Claude, SetupMark::Missing, "Claude Code");
    "Claude Code · not installed".clone_into(&mut item.summary);
    item.explanation = Some(
        "Needed for the Claude engine. Until it's set up, routing uses your other \
         accounts."
            .to_owned(),
    );
    if let Some(brew) = &facts.homebrew {
        item.state = vec![Styled::new("not installed", Tone::Error)];
        item.actions = vec![
            run_action(
                "Install with Homebrew",
                "brew install --cask claude-code",
                vec![
                    brew.path.display().to_string(),
                    "install".to_owned(),
                    "--cask".to_owned(),
                    "claude-code".to_owned(),
                ],
                "install",
            ),
            copy_action("brew install --cask claude-code"),
            later(),
        ];
        item.fix = Some(fix(
            "Install it with Homebrew",
            "brew install --cask claude-code",
        ));
    } else {
        item.state = vec![
            Styled::new("not installed", Tone::Error),
            Styled::new(" · Homebrew isn't installed either", Tone::Text),
        ];
        item.steps = vec![SetupStep::new("1. Install Claude Code", CLAUDE_INSTALL)];
        item.fix = Some(format!(
            "Install it with {CLAUDE_INSTALL}; Open shows the command to copy."
        ));
        item.footnote =
            Some("Or install it with Homebrew. I'll check again when you're back.".to_owned());
    }
    item
}

fn claude_item(facts: &SetupFacts) -> SetupItem {
    let login = run_action(
        "Log in to Claude",
        "claude auth login",
        vec![
            program(facts.paths.claude.as_ref(), "claude"),
            "auth".to_owned(),
            "login".to_owned(),
        ],
        "log in",
    );
    match &facts.claude {
        ToolState::Missing => claude_missing_item(facts),
        ToolState::LoggedOut { .. } | ToolState::Installed { .. } => {
            let mut item = SetupItem::new(SetupTool::Claude, SetupMark::Warn, "Claude Code");
            let words = if matches!(facts.claude, ToolState::LoggedOut { .. }) {
                "installed, not logged in"
            } else {
                "installed, its login couldn't be read"
            };
            item.state = vec![
                Styled::new(words, Tone::Warn),
                Styled::new(" · Claude models are unavailable on this Mac", Tone::Text),
            ];
            item.summary = format!("Claude Code · {words}");
            item.actions = vec![login];
            item.fix = Some(fix("Log in to Claude", "claude auth login"));
            item.hint = Some("runs claude auth login in a pane".to_owned());
            item
        }
        ToolState::ApiKey { version } => {
            let mut item = SetupItem::new(SetupTool::Claude, SetupMark::Warn, "Claude Code");
            item.state = vec![
                Styled::new("logged in with an API key", Tone::Warn),
                Styled::new(" · the app doesn't use it", Tone::Text),
            ];
            item.summary = format!("Claude Code {version} · logged in with an API key");
            item.explanation = Some(
                "The Claude engine runs only on a Claude subscription. To use this key, connect \
                 it under Settings ▸ Accounts, where it runs on the provider engine."
                    .to_owned(),
            );
            item.actions = vec![login, later()];
            item.hint = Some("runs claude auth login in a pane".to_owned());
            item.fix = Some(fix(
                "Log in with a Claude subscription",
                "claude auth login",
            ));
            item
        }
        ToolState::LoggedIn { version, account } => {
            let mut item = SetupItem::new(
                SetupTool::Claude,
                SetupMark::Ok,
                format!("Claude Code {version}"),
            );
            item.state = vec![dim(format!(
                "logged in · {account} → Claude engine available"
            ))];
            item.summary = format!("Claude Code {version} · logged in");
            item.details = vec![account.clone()];
            item
        }
    }
}

fn gh_item(facts: &SetupFacts) -> SetupItem {
    let explanation = "Needed for pull requests, branch delivery, cloning from GitHub and CI \
                       results. Until it's set up, projects here can only use build-in-place \
                       delivery.";
    match &facts.gh {
        ToolState::Missing => {
            let mut item = SetupItem::new(SetupTool::Gh, SetupMark::Missing, "GitHub CLI (gh)");
            "GitHub CLI · not installed".clone_into(&mut item.summary);
            if let Some(brew) = &facts.homebrew {
                item.state = vec![Styled::new("not installed", Tone::Error)];
                item.explanation = Some(explanation.to_owned());
                item.actions = vec![
                    run_action(
                        "Install with Homebrew",
                        "brew install gh",
                        vec![
                            brew.path.display().to_string(),
                            "install".to_owned(),
                            "gh".to_owned(),
                        ],
                        "install",
                    ),
                    copy_action("brew install gh"),
                    later(),
                ];
                item.fix = Some(fix("Install it with Homebrew", "brew install gh"));
            } else {
                item.state = vec![
                    Styled::new("not installed", Tone::Error),
                    Styled::new(" · Homebrew isn't installed either", Tone::Text),
                ];
                item.steps = vec![
                    SetupStep::new("1. Install Homebrew", HOMEBREW_INSTALL),
                    SetupStep::new("2. Install gh", "brew install gh"),
                ];
                item.fix = Some(
                    "Install Homebrew, then run brew install gh; Open shows the commands to copy."
                        .to_owned(),
                );
                item.footnote = Some(
                    "Or download the installer from cli.github.com. I'll check again when \
                         you're back."
                        .to_owned(),
                );
            }
            item
        }
        ToolState::LoggedIn { version, account } => {
            let mut item = SetupItem::new(
                SetupTool::Gh,
                SetupMark::Ok,
                format!("GitHub CLI {version}"),
            );
            item.state = vec![dim(if account.is_empty() {
                "logged in".to_owned()
            } else {
                format!("logged in as {account}")
            })];
            item.summary = format!("GitHub CLI {version} · logged in");
            if !facts.gh_scopes.is_empty() {
                item.details = vec![format!("scopes: {}", facts.gh_scopes.join(", "))];
            }
            item
        }
        state => {
            let mut item = SetupItem::new(SetupTool::Gh, SetupMark::Warn, "GitHub CLI (gh)");
            item.state = vec![
                Styled::new("installed, not logged in", Tone::Warn),
                Styled::new(
                    " · pull requests and CI are unavailable on this Mac",
                    Tone::Text,
                ),
            ];
            item.summary = format!(
                "GitHub CLI {} · not logged in",
                state.version().unwrap_or_default()
            );
            item.explanation = Some(explanation.to_owned());
            item.actions = vec![
                run_action(
                    "Log in to GitHub",
                    "gh auth login --web",
                    vec![
                        program(facts.paths.gh.as_ref(), "gh"),
                        "auth".to_owned(),
                        "login".to_owned(),
                        "--web".to_owned(),
                    ],
                    "log in",
                ),
                later(),
            ];
            item.hint = Some("runs gh auth login in a pane".to_owned());
            item.fix = Some(fix("Log in to GitHub", "gh auth login --web"));
            item
        }
    }
}

/// The setup screen for `facts`: items in the order git, Claude Code, GitHub CLI, the ones that
/// are fine first, then what's missing, then what needs a login.
pub fn view(facts: &SetupFacts) -> SetupView {
    let mut view = SetupView::default();
    view.checked = facts.checked;
    view.homebrew = facts
        .homebrew
        .as_ref()
        .map(|b| format!("Homebrew {} found at {}", b.version, b.prefix.display()));
    if !facts.checked {
        return view;
    }
    let mut items = vec![
        (git_item(facts), &facts.git),
        (claude_item(facts), &facts.claude),
        (gh_item(facts), &facts.gh),
    ]
    .into_iter()
    .map(|(mut item, state)| {
        item.version = state.version().map(str::to_owned);
        item
    })
    .collect::<Vec<_>>();
    items.sort_by_key(|item| match item.mark {
        SetupMark::Ok => 0,
        SetupMark::Missing => 1,
        _ => 2,
    });
    view.items = items;
    view
}

/// Runs the check in the background and publishes what it found.
#[derive(Clone)]
pub struct SetupCheck {
    inner: Arc<CheckInner>,
}

struct CheckInner {
    /// Paces the background re-checks.
    clock: Arc<dyn Clock>,
    env: BTreeMap<String, String>,
    facts: watch::Sender<SetupFacts>,
    /// One check at a time.
    running: tokio::sync::Mutex<()>,
    /// How many checks have finished.
    generation: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for SetupCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupCheck")
            .field("facts", &*self.inner.facts.borrow())
            .finish_non_exhaustive()
    }
}

impl SetupCheck {
    /// A check that runs its commands with `env` (the app's environment).
    pub fn new(clock: Arc<dyn Clock>, env: BTreeMap<String, String>) -> Self {
        Self {
            inner: Arc::new(CheckInner {
                clock,
                env,
                facts: watch::Sender::new(SetupFacts::default()),
                running: tokio::sync::Mutex::new(()),
                generation: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// The latest facts, updated after every check.
    pub fn facts(&self) -> watch::Receiver<SetupFacts> {
        self.inner.facts.subscribe()
    }

    /// Runs the check (off the async threads) and publishes what it found.
    pub async fn run(&self) -> SetupFacts {
        let _one = self.inner.running.lock().await;
        let env = self.inner.env.clone();
        let facts = match tokio::task::spawn_blocking(move || detect(&env)).await {
            Ok(facts) => facts,
            Err(e) => {
                tracing::error!(error = %e, "the setup check panicked");
                return self.inner.facts.borrow().clone();
            }
        };
        tracing::info!(
            git = facts.git.word(),
            gh = facts.gh.word(),
            claude = facts.claude.word(),
            "setup check"
        );
        self.inner.facts.send_replace(facts.clone());
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        facts
    }
}

/// The API's end of the setup check. Checks run in the background and the UI gets a fresh
/// snapshot when each is done, so the API never waits on `gh` or `claude`.
#[derive(Debug)]
pub struct SetupPart {
    check: SetupCheck,
    conversation: Arc<Conversation>,
}

impl SetupPart {
    pub fn new(check: SetupCheck, conversation: Arc<Conversation>) -> Self {
        Self {
            check,
            conversation,
        }
    }

    /// Runs the check now and then every [`RECHECK_EVERY`], in the background (it never blocks
    /// startup). Call it once.
    pub fn start(&self) {
        let check = self.check.clone();
        let conversation = Arc::clone(&self.conversation);
        let clock = Arc::clone(&self.check.inner.clock);
        tokio::spawn(async move {
            loop {
                check.run().await;
                conversation.changed();
                clock.sleep(RECHECK_EVERY).await;
            }
        });
    }

    /// Runs the check once more in the background, then asks for a fresh snapshot.
    fn recheck(&self) {
        let check = self.check.clone();
        let conversation = Arc::clone(&self.conversation);
        tokio::spawn(async move {
            check.run().await;
            conversation.changed();
        });
    }
}

impl ApiPart for SetupPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::RecheckSetup => Some(Box::pin(async move {
                self.recheck();
                Handled::Unchanged
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let facts = self.check.inner.facts.borrow().clone();
            snapshot.setup_issues = facts.issues();
            snapshot.setup = view(&facts);
            snapshot.setup.generation = self
                .check
                .inner
                .generation
                .load(std::sync::atomic::Ordering::SeqCst);
        })
    }
}
