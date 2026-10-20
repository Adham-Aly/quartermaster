//! How the app's own network git commands travel, whatever the repository's config says.
//!
//! A task's worktree shares the repository's `config` with your checkout (git keeps one per
//! repository), so a single `git config` run in any worktree changes how every git command in
//! the repository talks to a remote: another ssh program, a proxy, a certificate authority,
//! another transport. The app's own network commands (its fetches, `ls-remote`s and pushes) must
//! not follow such a setting, so every one of them is built here, by [`Repo::network_command`],
//! and nowhere else: the runner gives the longer network time limit only to a command carrying a
//! [`Transport`], which only this module makes.
//!
//! What git lets the app override, it overrides ([`Repo::network_command`] lists each); what git
//! gives no override for, it refuses ([`GitError::RepoTransportConfig`]), naming the setting, the
//! file it's in and the command that removes it. With a clean config nothing changes: your own
//! ssh command, credential helpers and protocols are used exactly as before.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::GitError;
use crate::git::Invocation;
use crate::repo::Repo;

/// The ssh the app runs when your own (global or system) git config names none: batch mode, so
/// a missing key fails at once instead of asking for a passphrase.
const APP_SSH: &str = "ssh -o BatchMode=yes";

/// Keys that change how a network command travels and that git lets no `-c` or variable
/// override (an empty value doesn't mean "the default" for them): set in the repository's own
/// config they stop an app network command ([`GitError::RepoTransportConfig`]). `{remote}` is
/// the remote, escaped.
const REPO_ONLY_TRANSPORT_KEYS: &str =
    r"^(core\.gitproxy|remote\.{remote}\.(proxy|vcs)|http\.(.*\.)?(proxy|sslcainfo|sslcapath))$";

/// The same for a `file` URL, where git runs `uploadpack` and `receivepack` itself, and keeps
/// the first value it reads, so the repository's beats any `-c`.
const REPO_ONLY_LOCAL_TRANSPORT_KEYS: &str = r"^(core\.gitproxy|remote\.{remote}\.(proxy|vcs|uploadpack|receivepack)|http\.(.*\.)?(proxy|sslcainfo|sslcapath))$";

/// Matches every credential helper setting, plain or URL-scoped.
const CREDENTIAL_HELPER_KEYS: &str = r"^credential\.(.+\.)?helper$";

/// A setting in the repository's own config (local or worktree scope, or a file either
/// includes) that would change how an app network command travels and that the app can't
/// override, so the command was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoTransportSetting {
    /// The key as git names it, lowercased where git lowercases (`http.proxy`,
    /// `remote.origin.vcs`, `http.https://example.com/.proxy`).
    pub key: String,
    /// The config file that sets it.
    pub file: PathBuf,
}

impl RepoTransportSetting {
    /// The settings as a sentence names them: `http.proxy (in /src/app/.git/config)`.
    pub fn listed(settings: &[Self]) -> String {
        sentence_list(
            settings
                .iter()
                .map(|s| format!("{} (in {})", s.key, s.file.display()))
                .collect(),
        )
    }

    /// The commands that remove them, as a sentence gives them:
    /// `` `git config --file /src/app/.git/config --unset-all http.proxy` ``.
    pub fn removal(settings: &[Self]) -> String {
        sentence_list(
            settings
                .iter()
                .map(|s| {
                    format!(
                        "`git config --file {} --unset-all {}`",
                        shell_quote(&s.file.to_string_lossy()),
                        shell_quote(&s.key)
                    )
                })
                .collect(),
        )
    }
}

/// `a`, `a and b`, `a, b and c`.
fn sentence_list(mut items: Vec<String>) -> String {
    match items.len() {
        0 => String::new(),
        1 => items.remove(0),
        _ => {
            let last = items.pop().unwrap_or_default();
            format!("{} and {last}", items.join(", "))
        }
    }
}

/// How an app network command authenticates over https.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Credentials<'a> {
    /// Your own credential helpers (system, global, and git's built-in ones), never the
    /// repository's: what the app's fetches have always used, minus anything a worktree wrote.
    Yours,
    /// For an `https://` URL, only `gh auth git-credential`, so a push and the API act as the same
    /// GitHub identity; for any other URL, [`Credentials::Yours`].
    Gh(&'a Path),
}

/// What makes a git command a network command: the app's own transport settings. Only this
/// module makes one, and the runner gives the network time limit only to a command carrying it.
#[derive(Debug)]
pub(crate) struct Transport {
    env: Vec<(&'static str, String)>,
    config: Vec<String>,
}

impl Transport {
    /// Variables set on the command (after the inherited `GIT_*` ones were removed).
    pub(crate) fn env(&self) -> &[(&'static str, String)] {
        &self.env
    }

    /// `-c` values, given after the app's own fixed ones, so they win over every config file.
    pub(crate) fn config(&self) -> &[String] {
        &self.config
    }
}

/// The transport git uses for `url`, as git decides it: `<helper>::…` names a remote helper,
/// `<scheme>://…` a scheme (`git+ssh` and `ssh+git` are ssh), `host:path` with no `/` before
/// the `:` is ssh, anything else a local path.
fn url_protocol(url: &str) -> String {
    let name_len = url
        .char_indices()
        .take_while(|&(i, c)| {
            c.is_ascii_alphabetic() || (i > 0 && (c.is_ascii_digit() || "+.-".contains(c)))
        })
        .count();
    let (name, rest) = url.split_at(name_len);
    if name_len > 0 && rest.starts_with("::") {
        return name.to_ascii_lowercase();
    }
    if name_len > 0 && rest.starts_with("://") {
        let scheme = name.to_ascii_lowercase();
        return match scheme.as_str() {
            "git+ssh" | "ssh+git" => "ssh".to_owned(),
            _ => scheme,
        };
    }
    match (url.find(':'), url.find('/')) {
        (Some(colon), slash) if slash.is_none_or(|s| s > colon) => "ssh".to_owned(),
        _ => "file".to_owned(),
    }
}

/// Git's own built-in policy for `protocol` (`protocol.<name>.allow`), which the app restores
/// over whatever the repository's config says: the known-safe transports always, `ext` (which
/// runs a command) never, anything else only when you run git directly, as the app does.
fn builtin_protocol_policy(protocol: &str) -> &'static str {
    match protocol {
        "http" | "https" | "git" | "ssh" => "always",
        "ext" => "never",
        _ => "user",
    }
}

/// `s` as a literal inside a POSIX extended regular expression.
fn regex_literal(s: &str) -> String {
    s.chars().fold(String::new(), |mut out, c| {
        if r".\^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// Quotes `s` for a POSIX shell when it needs it.
pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Whether a `--show-scope` scope is the repository's own (or the command line's), rather than
/// yours: git's built-in defaults show as `unknown`.
fn repository_scope(scope: &str) -> bool {
    matches!(scope, "local" | "worktree" | "command")
}

impl Repo {
    /// Builds `git <args>` as an app network command to `remote`, or refuses it.
    ///
    /// `url` is where the command goes, when the caller already checked it (a push's one push
    /// URL); without one it is the URL git fetches `remote` from (`git remote get-url`, which
    /// applies the repository's `insteadOf` rules exactly as `git fetch` and `git ls-remote`
    /// do).
    ///
    /// **Refused** ([`GitError::RepoTransportConfig`]) when the repository's own config (local
    /// or worktree scope, with what it includes) sets a key git gives no override for:
    /// `core.gitProxy`, `remote.<remote>.proxy`, `remote.<remote>.vcs`, or any `http.*proxy`,
    /// `http.*sslCAInfo`, `http.*sslCAPath`; and for a `file` URL also
    /// `remote.<remote>.uploadpack` and `.receivepack`, which git then runs on this Mac (for
    /// any other URL they only name the program the server runs) and where git keeps the first
    /// value it reads, so a `-c` can't replace the repository's.
    ///
    /// **Overridden**, as far as git allows:
    ///
    /// - **ssh:** `GIT_SSH_COMMAND` beats `core.sshCommand`, so it is always set: your own
    ///   `core.sshCommand` (global or system) when you have one, else batch-mode `ssh`; and
    ///   `GIT_SSH_VARIANT` beats `ssh.variant` (your own, else `ssh` or auto-detection).
    /// - **Asking for a password:** `GIT_ASKPASS` set but empty beats `core.askPass`, and with
    ///   terminal prompts off nothing is asked.
    /// - **The transport:** `protocol.allow=never` with only the URL's own protocol allowed,
    ///   under git's built-in policy for it, so a `remote.<name>.vcs`, a `protocol.*.allow` or
    ///   an `ext::` URL can't swap in another transport or run a command. With no URL nothing
    ///   is allowed.
    /// - **TLS:** `http.<url>.sslVerify=true`, keyed by the exact URL, beats any repository
    ///   `http.*.sslVerify` (the most specific match wins, and on a tie the last, which `-c`
    ///   is).
    /// - **Credentials:** `credential.helper=` empties every helper read so far, the
    ///   repository's URL-scoped ones too, then [`Credentials`] names the ones to use.
    ///
    /// URL rewriting isn't touched: a push checks its destination itself
    /// ([`Repo::push_urls`]), and a fetch only reads.
    pub(crate) fn network_command<I, S>(
        &self,
        remote: &str,
        url: Option<&str>,
        credentials: Credentials<'_>,
        args: I,
    ) -> Result<Invocation<'_>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let url = match url {
            Some(url) => Some(url.to_owned()),
            None => self.fetch_url(remote)?,
        };
        let protocol = url.as_deref().map(url_protocol);
        let settings = self.repo_transport_settings(remote, protocol.as_deref())?;
        if !settings.is_empty() {
            return Err(GitError::RepoTransportConfig {
                checkout: self.root().to_owned(),
                remote: remote.to_owned(),
                settings,
            });
        }
        let transport = self.app_transport(url.as_deref(), protocol.as_deref(), credentials)?;
        Ok(self.git().invoke(self.root(), args).over(transport))
    }

    /// The URL git fetches `remote` from (its first URL, rewritten), or `None` when it has
    /// none.
    fn fetch_url(&self, remote: &str) -> Result<Option<String>, GitError> {
        let out = self
            .git()
            .invoke(self.root(), ["remote", "get-url", remote])
            .output()?;
        let url = out.stdout.trim();
        Ok((out.success() && !url.is_empty()).then(|| url.to_owned()))
    }

    /// The transport settings described on [`Repo::network_command`].
    fn app_transport(
        &self,
        url: Option<&str>,
        protocol: Option<&str>,
        credentials: Credentials<'_>,
    ) -> Result<Transport, GitError> {
        let mut env = vec![("GIT_ASKPASS", String::new())];
        match self.user_config("core.sshCommand")? {
            Some(command) => {
                let variant = self
                    .user_config("ssh.variant")?
                    .unwrap_or_else(|| "auto".to_owned());
                env.extend([("GIT_SSH_COMMAND", command), ("GIT_SSH_VARIANT", variant)]);
            }
            None => env.extend([
                ("GIT_SSH_COMMAND", APP_SSH.to_owned()),
                ("GIT_SSH_VARIANT", "ssh".to_owned()),
            ]),
        }

        let mut config = vec!["protocol.allow=never".to_owned()];
        if let (Some(url), Some(protocol)) = (url, protocol) {
            config.push(format!(
                "protocol.{protocol}.allow={}",
                builtin_protocol_policy(protocol)
            ));
            if protocol == "https" {
                config.push(format!("http.{url}.sslVerify=true"));
            }
        }

        config.push("credential.helper=".to_owned());
        match credentials {
            Credentials::Gh(gh) if protocol == Some("https") => {
                config.push(format!(
                    "credential.helper=!{} auth git-credential",
                    shell_quote(&gh.to_string_lossy())
                ));
            }
            Credentials::Gh(_) | Credentials::Yours => {
                config.extend(self.user_credential_helpers()?);
            }
        }
        Ok(Transport { env, config })
    }

    /// A setting from your own git config only (global, else system), never the repository's.
    fn user_config(&self, key: &str) -> Result<Option<String>, GitError> {
        for scope in ["--global", "--system"] {
            let out = self
                .git()
                .invoke(self.root(), ["config", scope, "--get", key])
                .output()?;
            let value = out.stdout.trim();
            if out.success() && !value.is_empty() {
                return Ok(Some(value.to_owned()));
            }
        }
        Ok(None)
    }

    /// Your own credential helper settings, plain and URL-scoped, in git's order, as `-c`
    /// values (`credential.https://github.com.helper=!gh auth git-credential`): every one not
    /// from the repository's config.
    fn user_credential_helpers(&self) -> Result<Vec<String>, GitError> {
        let out = self
            .git()
            .invoke(
                self.root(),
                [
                    "config",
                    "--show-scope",
                    "-z",
                    "--get-regexp",
                    CREDENTIAL_HELPER_KEYS,
                ],
            )
            .output()?;
        match out.code {
            Some(0) => {}
            Some(1) => return Ok(Vec::new()),
            code => {
                return Err(GitError::Failed {
                    command: "config --get-regexp credential".to_owned(),
                    code,
                    stderr: out.stderr,
                });
            }
        }
        // `-z`: `<scope>\0<key>\n<value>\0` per entry (no `\n<value>` for a valueless key,
        // which git rejects for a helper anyway).
        let fields: Vec<&str> = out.stdout.split('\0').collect();
        Ok(fields
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|[scope, _]| !repository_scope(scope))
            .filter_map(|[_, entry]| entry.split_once('\n'))
            .map(|(key, value)| format!("{key}={value}"))
            .collect())
    }

    /// The settings of [`REPO_ONLY_TRANSPORT_KEYS`] (for a `file` URL,
    /// [`REPO_ONLY_LOCAL_TRANSPORT_KEYS`]) the repository's own config (local or worktree scope,
    /// with what it includes) sets for `remote`, each with its file.
    fn repo_transport_settings(
        &self,
        remote: &str,
        protocol: Option<&str>,
    ) -> Result<Vec<RepoTransportSetting>, GitError> {
        let keys = if protocol == Some("file") {
            REPO_ONLY_LOCAL_TRANSPORT_KEYS
        } else {
            REPO_ONLY_TRANSPORT_KEYS
        };
        let pattern = keys.replace("{remote}", &regex_literal(remote));
        let out = self
            .git()
            .invoke(
                self.root(),
                [
                    "config",
                    "--show-scope",
                    "--show-origin",
                    "--name-only",
                    "-z",
                    "--get-regexp",
                    pattern.as_str(),
                ],
            )
            .output()?;
        match out.code {
            Some(0) => {}
            Some(1) => return Ok(Vec::new()),
            code => {
                return Err(GitError::Failed {
                    command: "config --get-regexp".to_owned(),
                    code,
                    stderr: out.stderr,
                });
            }
        }
        // `-z --name-only`: `<scope>\0<origin>\0<key>\0` per entry; a file's origin is
        // `file:<path>`, relative to the checkout when git found it there.
        let fields: Vec<&str> = out.stdout.split('\0').collect();
        let mut settings: Vec<RepoTransportSetting> = Vec::new();
        for &[scope, origin, key] in fields.as_chunks::<3>().0 {
            if !matches!(scope, "local" | "worktree") {
                continue;
            }
            let path = Path::new(origin.strip_prefix("file:").unwrap_or(origin));
            let setting = RepoTransportSetting {
                key: key.to_owned(),
                file: self.root().join(path),
            };
            if !settings.contains(&setting) {
                settings.push(setting);
            }
        }
        Ok(settings)
    }
}
