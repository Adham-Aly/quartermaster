//! The hidden developer entry point.
//!
//! ```text
//! QM_DEV=1 QUARTERMASTER_HOME=<throwaway> quartermaster dev session
//!     --engine scripted|claude|provider  [--script FILE]  [--account REF] [--model ID]
//!     [--effort low|…|max] [--role quartermaster|crewmate] [--claude-bin PATH]
//!     [--idle-shutdown SECS] [--cwd DIR] [--resume SESSION]
//! QM_DEV=1 QUARTERMASTER_HOME=<dir> quartermaster dev login chatgpt --ref chatgpt:dev [--codex]
//! QM_DEV=1 QUARTERMASTER_HOME=<dir> quartermaster dev probe claude [--claude-bin PATH]
//! QM_DEV=1 QUARTERMASTER_HOME=<dir> quartermaster dev leftovers
//! ```
//!
//! **Gate.** Only with `QM_DEV=1` **and** an explicit `QUARTERMASTER_HOME` that isn't
//! `~/.quartermaster`; otherwise the binary behaves as it always does. It is in no help text. It
//! takes `run.lock`, starts logging and opens the store like a normal launch, so it exercises the
//! real startup.
//!
//! - [`session`]: one agent on a real engine (Claude with the OS supervisor recording into the
//!   store; the provider engine with the credential file, the built-in profiles and the file
//!   tools; the scripted engine from `--script`), driven from stdin, every event printed.
//! - [`login`]: Sign in with ChatGPT (or the Codex login) into `<home>/auth.json`.
//! - [`probe`]: detection and the startup check of `claude`.
//! - [`leftovers`]: the startup leftover report.

mod leftovers;
mod login;
mod probe;
mod session;

use crate::{Failure, home, lock, logging};
use qm_store::db::Db;
use qm_types::data_dir::DataDir;
use qm_types::env::QUARTERMASTER_HOME;
use std::path::{Path, PathBuf};

/// `Some` when this is a developer command (and the gate is open); the binary returns its result.
pub(crate) fn dispatch() -> Option<Result<(), Failure>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("dev")
        || std::env::var("QM_DEV").as_deref() != Ok("1")
    {
        return None;
    }
    let rest = &args[1..];
    let explicit = std::env::var_os(QUARTERMASTER_HOME).filter(|h| !h.is_empty())?;
    let dir = DataDir::resolve().ok()?;
    let _ = explicit;
    if is_real_home(dir.root()) {
        return None;
    }
    Some(run(&dir, rest))
}

/// Whether `root` is the real `~/.quartermaster`, which developer commands never touch.
fn is_real_home(root: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let real = home.join(".quartermaster");
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    canon(root) == canon(&real)
}

/// What every developer command shares: the data directory, the lock, logging and the store.
pub(super) struct Startup {
    pub dir: DataDir,
    pub db: Db,
    _lock: lock::RunLock,
}

fn fail(message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
    }
}

/// The normal launch's startup.
fn start(dir: &DataDir) -> Result<Startup, Failure> {
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    home::prepare(dir).map_err(|e| {
        fail(format!(
            "can't create the data directory {}: {e}",
            dir.root().display()
        ))
    })?;
    let lock = match lock::RunLock::acquire(&dir.run_lock()) {
        Ok(lock) => lock,
        Err(lock::LockError::AlreadyRunning) => {
            return Err(fail(format!(
                "Quartermaster is already running with the data directory {}",
                dir.root().display()
            )));
        }
        Err(e) => return Err(fail(format!("can't take the run lock: {e}"))),
    };
    logging::init(&dir.logs()).map_err(|e| fail(format!("can't start logging: {e}")))?;
    tracing::info!(data_dir = %dir.root().display(), "starting a developer command");
    let db =
        Db::open(&dir.database()).map_err(|e| fail(format!("can't open the database: {e}")))?;
    Ok(Startup {
        dir: dir.clone(),
        db,
        _lock: lock,
    })
}

fn run(dir: &DataDir, args: &[String]) -> Result<(), Failure> {
    let usage = || {
        fail(
            "usage: quartermaster dev session|login chatgpt|probe claude|leftovers \
             (see crates/quartermaster/src/dev/mod.rs)",
        )
    };
    let command = args.first().map(String::as_str).ok_or_else(usage)?;
    let flags = Flags::parse(&args[1..])?;
    let startup = start(dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("qm-dev")
        .build()
        .map_err(|e| fail(format!("can't start the runtime: {e}")))?;
    let result = match command {
        "session" => runtime.block_on(session::run(&startup, &flags)),
        "login" if flags.positional.first().map(String::as_str) == Some("chatgpt") => {
            runtime.block_on(login::run(&startup, &flags))
        }
        "probe" if flags.positional.first().map(String::as_str) == Some("claude") => {
            runtime.block_on(probe::run(&flags))
        }
        "leftovers" => runtime.block_on(leftovers::run(&startup)),
        _ => Err(usage()),
    };
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// `--name value` flags and bare words, in order.
#[derive(Debug, Default)]
pub(super) struct Flags {
    pub positional: Vec<String>,
    values: Vec<(String, Option<String>)>,
}

/// Flags that take no value.
const SWITCHES: &[&str] = &["--codex"];

impl Flags {
    fn parse(args: &[String]) -> Result<Self, Failure> {
        let mut flags = Self::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if let Some((name, value)) = arg.split_once('=')
                && name.starts_with("--")
            {
                flags.values.push((name.to_owned(), Some(value.to_owned())));
            } else if SWITCHES.contains(&arg.as_str()) {
                flags.values.push((arg.clone(), None));
            } else if arg.starts_with("--") {
                let value = it
                    .next()
                    .ok_or_else(|| fail(format!("{arg} needs a value")))?;
                flags.values.push((arg.clone(), Some(value.clone())));
            } else {
                flags.positional.push(arg.clone());
            }
        }
        Ok(flags)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }

    pub fn has(&self, name: &str) -> bool {
        self.values.iter().any(|(n, _)| n == name)
    }
}

/// The app's own environment, snapshotted once (engines copy only what they own from it).
pub(super) fn inherited_env() -> std::collections::BTreeMap<String, String> {
    std::env::vars().collect()
}

/// `ping { text }` → `pong from rust: <text>`: the prototype's tool. Built here, never added to the
/// registry.
pub(super) fn ping_tool() -> qm_engine::ToolDescriptor {
    qm_engine::ToolDescriptor {
        name: "ping".into(),
        description: "Echo a short text back. Used to prove the tool call reached the Rust host."
            .into(),
        input_schema: serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {"text": {"type": "string", "description": "Text to echo"}},
            "required": ["text"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn dev_fail(message: impl Into<String>) -> Failure {
    fail(message)
}
