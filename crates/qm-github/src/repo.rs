//! A repository on GitHub, and reading one from a remote URL.

use std::fmt;

/// `owner/name`. Compared case-insensitively, as GitHub does.
#[derive(Clone, Debug, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl PartialEq for RepoRef {
    fn eq(&self, other: &Self) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

fn valid_part(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

impl RepoRef {
    /// `owner/name`, as a tool argument gives it.
    pub fn parse(s: &str) -> Option<Self> {
        let (owner, name) = s.trim().split_once('/')?;
        (valid_part(owner) && valid_part(name)).then(|| Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The repository a GitHub remote URL points at, in any form git can push to GitHub with:
    ///
    /// - `https://github.com/o/r`, with or without a user (`https://you@github.com/…`) or port
    ///   443;
    /// - `ssh://git@github.com/o/r` (port 22), `ssh://git@ssh.github.com:443/o/r`, and the
    ///   `git+ssh://` and `ssh+git://` spellings;
    /// - scp-like `git@github.com:o/r` (or `git@ssh.github.com:o/r`).
    ///
    /// Schemes and hosts compare case-insensitively; one trailing `/` and then one `.git` are
    /// dropped. Anything else (an ssh `Host` alias, plain `http://` or `git://` among them) isn't
    /// taken for a GitHub remote, so an app push is only ever matched to forms that provably
    /// reach github.com.
    ///
    /// Anything a URL parser could read two ways is refused rather than guessed at: a `?` or
    /// `#` anywhere (git and curl end the host at `/`, `?` or `#`, so
    /// `https://evil.example#@github.com/o/r` connects to evil.example), more than one `@` before
    /// the host (`a@b@github.com`), a backslash, whitespace or a control character.
    pub fn from_remote(url: &str) -> Option<Self> {
        let url = url.trim();
        if url
            .chars()
            .any(|c| matches!(c, '?' | '#' | '\\') || c.is_whitespace() || c.is_control())
        {
            return None;
        }
        let path = if let Some((scheme, rest)) = url.split_once("://") {
            // No `?` or `#` is left, so the authority ends at the first `/`.
            let (authority, path) = rest.split_once('/')?;
            if authority.matches('@').count() > 1 {
                return None;
            }
            let (user, host_port) = match authority.split_once('@') {
                Some((user, host_port)) => (Some(user), host_port),
                None => (None, authority),
            };
            let (host, port) = match host_port.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (host_port, None),
            };
            let github = host.eq_ignore_ascii_case("github.com");
            let ssh_github = host.eq_ignore_ascii_case("ssh.github.com");
            let ok = match scheme.to_ascii_lowercase().as_str() {
                "https" => github && port.is_none_or(|p| p == "443"),
                "ssh" | "git+ssh" | "ssh+git" => {
                    user == Some("git")
                        && ((github && port.is_none_or(|p| p == "22"))
                            || (ssh_github && port.is_none_or(|p| p == "22" || p == "443")))
                }
                _ => false,
            };
            if !ok {
                return None;
            }
            path
        } else {
            // Git reads `user@host:path` as ssh only when no `/` comes before the `:`.
            let (user_host, path) = url.split_once(':')?;
            if user_host.contains('/') || user_host.matches('@').count() != 1 {
                return None;
            }
            let (user, host) = user_host.split_once('@')?;
            let ok = user == "git"
                && (host.eq_ignore_ascii_case("github.com")
                    || host.eq_ignore_ascii_case("ssh.github.com"));
            if !ok {
                return None;
            }
            path
        };
        let path = path.strip_suffix('/').unwrap_or(path);
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
        (valid_part(owner) && valid_part(name)).then(|| Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// A project's GitHub repository as its row stores it: `owner/name` (what registering a
    /// project records), or a remote URL.
    pub fn from_project(stored: &str) -> Option<Self> {
        Self::parse(stored).or_else(|| Self::from_remote(stored))
    }

    /// This repository's `https` URL, for a sentence telling you how to point a remote back at
    /// it.
    pub fn https_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.name)
    }

    /// As `qm_policy::github::Repo` wants it.
    pub fn as_policy(&self) -> (&str, &str) {
        (&self.owner, &self.name)
    }

    /// The API path of the repository: `/repos/o/r`.
    pub(crate) fn path(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.name)
    }
}

/// `url` safe to show: a password or token written into its user part (`https://user:token@…`,
/// `https://token@…`) is dropped. An ssh user name (`ssh://git@…`, `git@host:…`) is kept. The
/// host is read as git and curl read it (the authority ends at `/`, `?` or `#`; the user part
/// ends at its last `@`), so the URL shown names the host it really connects to.
pub fn redact_url(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let Some((user, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let ssh = matches!(
        scheme.to_ascii_lowercase().as_str(),
        "ssh" | "git+ssh" | "ssh+git"
    );
    let kept = match user.split_once(':') {
        _ if !ssh => "",
        Some((name, _)) => name,
        None => user,
    };
    let at = if kept.is_empty() { "" } else { "@" };
    format!("{scheme}://{kept}{at}{host}{tail}")
}
