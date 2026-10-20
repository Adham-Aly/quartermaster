//! Finding `claude` and its login.
//!
//! `claude` is looked for on the app's inherited `PATH`, then at `~/.local/bin/claude`. Its
//! version comes from `claude --version` (the first `x.y.z`), its login from `claude auth status
//! --json`, both run with the environment an agent gets ([`crate::env::base_env`]) so they see
//! the same login an agent will. Only a logged-in claude.ai subscription makes the Claude engine
//! available. Nothing here ever prints or keeps an account detail.

use crate::env::base_env;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one detection command may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// How `claude` is logged in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginState {
    /// A claude.ai subscription (`max`, `pro`, …): the Claude engine is available.
    Subscription {
        subscription_type: String,
    },
    /// Logged in with an API key: not used (never per-token billing).
    ApiKey,
    LoggedOut,
    /// `claude` is missing, or its login couldn't be read.
    Missing,
}

/// What detection found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeInstall {
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    pub login: LoginState,
}

impl ClaudeInstall {
    /// Why the Claude engine can't run, in one plain sentence; `None` when it can.
    pub fn unavailable_reason(&self) -> Option<String> {
        match (&self.path, &self.login) {
            (None, _) => Some("Claude Code isn't installed on this Mac, so the Claude engine is off.".into()),
            (Some(_), LoginState::Subscription { .. }) => None,
            (Some(_), LoginState::ApiKey) => Some(
                "Claude Code is logged in with an API key, not a Claude subscription, so the Claude engine is off.".into(),
            ),
            (Some(_), LoginState::LoggedOut) => Some(
                "Claude Code isn't logged in; run `claude` and log in with your Claude subscription to turn the Claude engine on.".into(),
            ),
            (Some(_), LoginState::Missing) => Some(
                "Claude Code's login couldn't be read, so the Claude engine is off.".into(),
            ),
        }
    }
}

/// Finds `claude` on the inherited `PATH` or at `~/.local/bin/claude`, then detects it.
pub fn detect(inherited_env: &BTreeMap<String, String>) -> ClaudeInstall {
    match find_claude(inherited_env) {
        Some(path) => detect_at(&path, inherited_env),
        None => ClaudeInstall {
            path: None,
            version: None,
            login: LoginState::Missing,
        },
    }
}

/// Detects the `claude` at `path`.
pub fn detect_at(path: &Path, inherited_env: &BTreeMap<String, String>) -> ClaudeInstall {
    let env = base_env(inherited_env);
    let cwd = inherited_env
        .get("HOME")
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let version = run(path, &["--version"], &env, &cwd).and_then(|out| parse_version(&out));
    let login = match run(path, &["auth", "status", "--json"], &env, &cwd) {
        Some(out) => parse_login(&out),
        None => LoginState::Missing,
    };
    ClaudeInstall {
        path: Some(path.to_owned()),
        version,
        login,
    }
}

fn find_claude(inherited_env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(path) = inherited_env.get("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let candidate = Path::new(dir).join("claude");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let home = inherited_env.get("HOME")?;
    let candidate = Path::new(home).join(".local/bin/claude");
    is_executable(&candidate).then_some(candidate)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Runs one command with a timeout; its stdout when it exits successfully.
fn run(
    program: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
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
    let out = reader.join().ok()?;
    status
        .filter(std::process::ExitStatus::success)
        .map(|_| out)
}

/// The first `x.y.z` in `claude --version` output (`2.1.303 (Claude Code)`).
pub fn parse_version(output: &str) -> Option<String> {
    output
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let parts: Vec<&str> = word.split('.').collect();
            (parts.len() >= 3 && parts[..3].iter().all(|p| !p.is_empty()))
                .then(|| parts[..3].join("."))
        })
}

/// Reads `claude auth status --json`. Only `loggedIn: true` with `authMethod: "claude.ai"` and a
/// `subscriptionType` is a subscription.
pub fn parse_login(output: &str) -> LoginState {
    let Ok(status) = serde_json::from_str::<Value>(output.trim()) else {
        return LoginState::Missing;
    };
    if status.get("loggedIn").and_then(Value::as_bool) != Some(true) {
        return LoginState::LoggedOut;
    }
    let method = status.get("authMethod").and_then(Value::as_str);
    let subscription = status
        .get("subscriptionType")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match (method, subscription) {
        (Some("claude.ai"), Some(subscription)) => LoginState::Subscription {
            subscription_type: subscription.to_owned(),
        },
        (Some("claude.ai"), None) => LoginState::Missing,
        _ => LoginState::ApiKey,
    }
}

/// Whether `version` is in `[min, max)`, comparing `x.y.z` numerically.
pub fn version_in_range(version: &str, (min, max): (&str, &str)) -> bool {
    match (parse_triple(version), parse_triple(min), parse_triple(max)) {
        (Some(v), Some(lo), Some(hi)) => v >= lo && v < hi,
        _ => false,
    }
}

fn parse_triple(version: &str) -> Option<(u64, u64, u64)> {
    let version = parse_version(version)?;
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}
