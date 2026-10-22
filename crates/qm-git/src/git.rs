//! The typed runner over your installed `git`.
//!
//! Every command the app itself runs goes through [`Git`], which makes it safe and predictable:
//!
//! - every inherited `GIT_*` variable is removed, so a `GIT_DIR` or `GIT_INDEX_FILE` left in the
//!   shell that started the app can never redirect it to another repository;
//! - hooks are skipped (`core.hooksPath=/dev/null`): the project's hooks are for its developers'
//!   and crewmates' own commits, never for the app's bookkeeping;
//! - no other program the repository's config names runs either ([`APP_CONFIG`],
//!   [`Invocation::driver_overrides`]): a task's worktree shares that config with your checkout,
//!   so a crewmate can write it, and the app's local commands (status, merge, diff, worktrees)
//!   must not run what it says: no fsmonitor hook, content filter, merge or diff driver, signing
//!   program, editor or pager, and no lazy fetch from a partial clone's remote, whichever file
//!   (or file it includes) sets them;
//! - output is parse-stable (`LC_ALL=C`, no colour, no pager, unquoted paths) and only porcelain
//!   or plumbing forms are parsed;
//! - nothing ever prompts: standard input is closed and `GIT_TERMINAL_PROMPT=0`;
//! - network commands (fetch, `ls-remote`, push) travel the app's own way, never the way the
//!   repository's config (shared with every task worktree) says: only `Repo::network_command`
//!   (`network.rs`) builds one, pinning ssh, the protocol, TLS and credentials and refusing
//!   what git gives no override for;
//! - optional locks are off (`GIT_OPTIONAL_LOCKS=0`), so reading status never fights a
//!   crewmate's git for the index;
//! - automatic `gc` and maintenance are off for the app's commands, so a bookkeeping step never
//!   turns into a long background job;
//! - every command has a time limit and is stopped (with everything it started) past it.
//!
//! Commands run synchronously on the calling thread. The core calls these from
//! `tokio::task::spawn_blocking`, since a fetch can take a while.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::error::GitError;
use crate::network::{Transport, shell_quote};
use crate::process::{self, Captured, OutputMode};

/// Time limit for local commands (refs, worktrees, status).
const LOCAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Time limit for commands that talk to a remote (`fetch`, `ls-remote`).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(600);

/// The most output kept from one git command.
const OUTPUT_CAP: usize = 64 * 1024 * 1024;

/// Configuration every app-run git command gets on its command line.
const APP_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "color.ui=never",
    "core.quotePath=false",
    "core.pager=cat",
    "advice.detachedHead=false",
    "gc.auto=0",
    "maintenance.auto=false",
    // Programs the repository's config could name. An fsmonitor hook runs on every status; `:` is
    // git's own "no editor"; nothing the app runs signs or verifies a signature, so the signing
    // programs, which git would otherwise take from the repository, can only fail; and credential
    // helpers are emptied (only a network command gets helpers back, its own, from `network.rs`).
    "core.fsmonitor=false",
    "core.editor=:",
    "sequence.editor=:",
    "commit.gpgSign=false",
    "tag.gpgSign=false",
    "tag.forceSignAnnotated=false",
    "push.gpgSign=false",
    "merge.verifySignatures=false",
    "log.showSignature=false",
    "gpg.program=/usr/bin/false",
    "gpg.openpgp.program=/usr/bin/false",
    "gpg.x509.program=/usr/bin/false",
    "gpg.ssh.program=/usr/bin/false",
    "gpg.ssh.defaultKeyCommand=/usr/bin/false",
    "credential.helper=",
    // A fetch from a repository with alternates runs this to list the alternates' refs; `true`
    // lists none (only a slower negotiation; an empty value is an error, not "none").
    "core.alternateRefsCommand=true",
    // A submodule's working tree is looked into by running git there under its own config; see
    // `SUBMODULE_COMMANDS` and `Invocation::driver_overrides`.
    "diff.ignoreSubmodules=dirty",
];

/// Settings that make a local command run a program, one driver name at a time, so no fixed
/// `-c` can cover them: content filters and merge drivers. [`Invocation::driver_overrides`]
/// reads which names the config sets before each command, and the submodules it names
/// (`submodule.<name>.*`).
const DRIVER_KEYS: &str =
    r"^(filter\..+\.(clean|smudge|process|required)|merge\..+\.driver|submodule\..+\.[^.]+)$";

/// A project's submodules as `.gitmodules` names them.
const GITMODULES_KEYS: &str = r"^submodule\..+\.path$";

/// Subcommands that can show a patch, and so run an external diff program or a textconv
/// driver: each gets `--no-ext-diff --no-textconv` (a `diff.external` or a driver's `command`
/// or `textconv` set to nothing is an error, not "none").
const PATCH_COMMANDS: &[&str] = &["diff", "log", "show"];

/// Subcommands that look inside a submodule's working tree for changes, by running `git status`
/// there under the submodule's own config (`.git/modules/<name>/config`, which the drivers'
/// overrides don't read): each gets `--ignore-submodules=dirty`, which only the command line can
/// set over a `submodule.<name>.ignore`. A submodule moved to another commit still shows; what
/// is uncommitted inside one doesn't, and nothing the app does touches it.
const SUBMODULE_COMMANDS: &[&str] = &["status", "diff"];

/// Your installed `git`, run the way the app's own commands must run.
///
/// Cheap to clone; holds only the program to run.
#[derive(Clone, Debug)]
pub struct Git {
    program: PathBuf,
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Git {
    /// The `git` found on `PATH`, the one you use yourself.
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("git"),
        }
    }

    /// A specific `git` binary, for a Mac where startup detection found it off `PATH`.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// The program this runner starts.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// `git --version`, trimmed (for example `git version 2.50.1 (Apple Git-155)`), for the
    /// setup check's detection at startup.
    pub fn version(&self) -> Result<String, GitError> {
        let out = self.run(Path::new("/"), ["--version"])?;
        Ok(out.trim().to_owned())
    }

    /// Runs `git <args>` in `dir`; a non-zero exit is an error. Returns stdout as text.
    pub(crate) fn run<I, S>(&self, dir: &Path, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.invoke(dir, args).text()
    }

    /// Starts building an invocation of `git <args>` in `dir`.
    pub(crate) fn invoke<I, S>(&self, dir: &Path, args: I) -> Invocation<'_>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Invocation {
            git: self,
            dir: dir.to_owned(),
            args: args.into_iter().map(|a| a.as_ref().to_owned()).collect(),
            transport: None,
        }
    }
}

/// One git command about to run.
#[derive(Debug)]
pub(crate) struct Invocation<'a> {
    git: &'a Git,
    dir: PathBuf,
    args: Vec<OsString>,
    /// Set for a network command: the app's own transport, and the longer time limit.
    transport: Option<Transport>,
}

/// What a git command left behind, whatever its exit status.
#[derive(Debug)]
pub(crate) struct GitOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl Invocation<'_> {
    /// Makes this a network command travelling over `transport` (which only
    /// `Repo::network_command` makes), with the longer time limit.
    pub(crate) fn over(mut self, transport: Transport) -> Self {
        self.transport = Some(transport);
        self
    }

    /// The command as shown in errors: `fetch origin main`, never the `-c` prelude.
    fn display(&self) -> String {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Runs it, returning the output whatever the exit status. Spawn failures and time limits
    /// are still errors.
    pub(crate) fn output(self) -> Result<GitOutput, GitError> {
        let shown = self.display();
        let mut command = Command::new(&self.git.program);
        command.current_dir(&self.dir);
        scrub_git_env(&mut command);
        let drivers = self.driver_overrides();
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env_remove("LANGUAGE");
        if let Some(transport) = &self.transport {
            for (key, value) in transport.env() {
                command.env(key, value);
            }
        }
        command.arg("--no-pager");
        for config in APP_CONFIG {
            command.arg("-c").arg(config);
        }
        for config in &drivers {
            command.arg("-c").arg(config);
        }
        for config in self.transport.iter().flat_map(Transport::config) {
            command.arg("-c").arg(config);
        }
        let mut args = self.args.iter();
        if let Some(subcommand) = args.next() {
            command.arg(subcommand);
            if PATCH_COMMANDS.iter().any(|p| subcommand == *p) {
                command.args(["--no-ext-diff", "--no-textconv"]);
            }
            if SUBMODULE_COMMANDS.iter().any(|p| subcommand == *p) {
                command.arg("--ignore-submodules=dirty");
            }
        }
        command.args(args);

        let timeout = if self.transport.is_some() {
            NETWORK_TIMEOUT
        } else {
            LOCAL_TIMEOUT
        };
        tracing::debug!(dir = %self.dir.display(), command = %shown, "git");
        let Captured {
            code,
            stdout,
            stderr,
            timed_out,
        } = process::run(command, timeout, OutputMode::Separate, OUTPUT_CAP).map_err(|source| {
            GitError::Spawn {
                program: self.git.program.clone(),
                source,
            }
        })?;
        if timed_out {
            return Err(GitError::TimedOut {
                command: shown,
                after: timeout,
            });
        }
        Ok(GitOutput {
            code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        })
    }

    /// The `-c` values that stop the repository's config from running a filter or merge driver
    /// ([`DRIVER_KEYS`]): for each such key the repository's own config sets (local or worktree
    /// scope, with what they include), your own value (global or system) when you have one, so
    /// your Git LFS keeps working; else none (an empty filter command is no filter, and not
    /// required), and for a merge driver git's own text merge, by `git merge-file`. Read just
    /// before the command runs.
    ///
    /// The merge-file driver names its sides with fixed labels, not git's `%X`/`%S`/`%Y`
    /// placeholders, which older git (the Command Line Tools of earlier macOS releases) leaves
    /// as they are: the result is the same, and the app's only merge, `merge_in_place`, aborts
    /// on a conflict, so no conflict marker it writes is ever left in your checkout.
    ///
    /// Every submodule the repository's config or its `.gitmodules` names also gets
    /// `submodule.<name>.ignore=dirty`: git looks for changes inside a submodule's working tree
    /// by running `git status` there, under the submodule's own config, which this doesn't read
    /// (`git merge` does it too, through `git stash create`). The per-name setting is the only
    /// `-c` that beats a `submodule.<name>.ignore` in `.gitmodules` or the config, and unlike the
    /// command line (`SUBMODULE_COMMANDS`) it reaches the git commands git itself starts.
    ///
    /// Empty for `git config` itself (which runs nothing) and `--version`, and when the config
    /// can't be read: then the command can't read it either, and stops before running anything.
    fn driver_overrides(&self) -> Vec<String> {
        let subcommand = self.args.first().map(|a| a.to_string_lossy());
        if matches!(subcommand.as_deref(), None | Some("config" | "--version")) {
            return Vec::new();
        }
        let out = match self
            .git
            .invoke(
                &self.dir,
                ["config", "--show-scope", "-z", "--get-regexp", DRIVER_KEYS],
            )
            .output()
        {
            Ok(out) if out.code == Some(0) => out,
            Ok(out) => {
                if out.code != Some(1) {
                    tracing::debug!(stderr = %out.stderr, "couldn't read the drivers' config");
                }
                return Vec::new();
            }
            Err(e) => {
                tracing::debug!(error = %e, "couldn't read the drivers' config");
                return Vec::new();
            }
        };
        let (mut overrides, mut submodules) = driver_overrides_from(&out.stdout, &self.git.program);
        submodules.extend(self.gitmodules_names());
        submodules.sort_unstable();
        submodules.dedup();
        overrides.extend(
            submodules
                .iter()
                .map(|name| format!("submodule.{name}.ignore=dirty")),
        );
        overrides
    }

    /// The submodule names in `.gitmodules` at the top of this command's folder, if it has one.
    fn gitmodules_names(&self) -> Vec<String> {
        if !self.dir.join(".gitmodules").is_file() {
            return Vec::new();
        }
        let listing = self
            .git
            .invoke(
                &self.dir,
                [
                    "config",
                    "--file",
                    ".gitmodules",
                    "-z",
                    "--name-only",
                    "--get-regexp",
                    GITMODULES_KEYS,
                ],
            )
            .output();
        match listing {
            Ok(out) if out.code == Some(0) => out
                .stdout
                .split('\0')
                .filter_map(|key| submodule_name(key).map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Runs it; a non-zero exit is [`GitError::Failed`]. Returns stdout as text.
    pub(crate) fn text(self) -> Result<String, GitError> {
        let shown = self.display();
        let out = self.output()?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(GitError::Failed {
                command: shown,
                code: out.code,
                stderr: out.stderr,
            })
        }
    }
}

/// [`Invocation::driver_overrides`] from `git config --show-scope -z --get-regexp` output:
/// `<scope>\0<key>\n<value>\0` per entry, or `<scope>\0<key>\0` for a key with no value
/// (which git reads as `true`). Also returns the submodules the repository's own config names.
fn driver_overrides_from(listing: &str, program: &Path) -> (Vec<String>, Vec<String>) {
    let fields: Vec<&str> = listing.split('\0').collect();
    let mut repository: Vec<&str> = Vec::new();
    let mut yours: Vec<(&str, Option<&str>)> = Vec::new();
    let mut submodules: Vec<String> = Vec::new();
    for [scope, entry] in fields.as_chunks::<2>().0 {
        let (key, value) = match entry.split_once('\n') {
            Some((key, value)) => (key, Some(value)),
            None => (*entry, None),
        };
        if let Some(name) = submodule_name(key) {
            if matches!(*scope, "local" | "worktree") {
                submodules.push(name.to_owned());
            }
            continue;
        }
        match *scope {
            "local" | "worktree" if !repository.contains(&key) => repository.push(key),
            "system" | "global" => yours.push((key, value)),
            _ => {}
        }
    }
    let overrides = repository
        .into_iter()
        .map(|key| match yours.iter().rev().find(|(k, _)| *k == key) {
            Some((_, Some(value))) => format!("{key}={value}"),
            Some((_, None)) => key.to_owned(),
            None if key.ends_with(".required") => format!("{key}=false"),
            None if key.starts_with("merge.") => format!(
                "{key}={} merge-file -L HEAD -L base -L incoming --marker-size=%L %A %O %B",
                shell_quote(&program.to_string_lossy())
            ),
            None => format!("{key}="),
        })
        .collect();
    (overrides, submodules)
}

/// The submodule a `submodule.<name>.<variable>` key names (a name may hold dots; a variable
/// can't).
fn submodule_name(key: &str) -> Option<&str> {
    key.strip_prefix("submodule.")?
        .rsplit_once('.')
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
}

/// Removes every inherited `GIT_*` variable from `command`'s environment.
pub(crate) fn scrub_git_env(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
}
