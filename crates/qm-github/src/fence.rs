//! The credentials fence.
//!
//! A crewmate's processes must not be able to push or write to GitHub. Only the app does that,
//! after its checks, in its own environment with its own token. A crewmate never needs to push
//! anywhere, so the fence blocks every network push, whatever the host, not only GitHub's URL
//! forms. Fetching keeps working over https and ssh, with or without a port. [`apply`] adds the
//! fence to a crewmate's environment (`qm-core`'s `tasks/env.rs` calls it), and [`check`] holds an
//! environment to it. The layers are independent, so a form one layer misses is still caught by
//! another:
//!
//! 1. **No GitHub login.** `GH_CONFIG_DIR` is an empty folder, so `gh` is logged out and its
//!    `gh auth git-credential` helper answers nothing. No token variable is ever set.
//! 2. **Every push URL rewritten.** Environment-only git config (`GIT_CONFIG_COUNT`,
//!    `GIT_CONFIG_KEY_n`, `GIT_CONFIG_VALUE_n`) sets `url.<fence>.pushInsteadOf` for the
//!    prefixes in [`PUSH_URLS`]:
//!    - every https and http URL, by its scheme alone (`https://`, `http://`, `HTTPS://`,
//!      `HTTP://`), for any host, user or port;
//!    - every `ssh://`, `git+ssh://`, `ssh+git://` and `git://` URL;
//!    - GitHub's scp-like forms.
//!
//!    The fence ([`PUSH_FENCE_PREFIX`]) isn't a repository, so `git push` fails with "pushes go
//!    through `pr_open`" in git's own error. `pushInsteadOf` applies only to pushes, so fetches
//!    are untouched.
//! 3. **The remote's push URL replaced.** `remote.origin.pushurl` points at the fence too, and
//!    `git push` and `git push origin` take it over the remote's own URL, whatever form that
//!    URL has.
//! 4. **Ssh pushes refused at the transport.** `GIT_SSH_COMMAND` is a small wrapper around `ssh`
//!    ([`SSH_FENCE_COMMAND`]). When git asks it to run `git-receive-pack` (a push) on any host,
//!    it refuses with "pushes go through `pr_open`". That covers every ssh URL form, scp-like
//!    `host:path` included, and every `Host` alias of your ssh config. `git-upload-pack` (a
//!    fetch) goes straight through to `ssh`. `GIT_SSH_VARIANT=ssh` tells git the wrapper takes
//!    OpenSSH's options. Without it, git treats a command not named `ssh` as the "simple" variant
//!    and refuses any ssh URL with a port, fetches included.
//!
//! What is deliberately left alone:
//!
//! - **The crewmate's credential helpers.** Emptying them would also break `git fetch` of a private
//!   repository over https, which must keep working.
//! - **The residual.** A push to an https URL whose scheme is deliberately written in mixed case
//!   (`hTTps://…`) is matched by no literal `pushInsteadOf` prefix. Git takes URL schemes
//!   case-insensitively, so it would leave. So would a push URL a process writes into a
//!   repository's own config, since an explicit `pushurl` isn't rewritten by `pushInsteadOf`. This
//!   is accepted: the fence is a guard rail against mistakes and habits, not a sandbox against a
//!   process set on getting around it. Such a push also needs your own keychain login to go
//!   anywhere.
//!
//! Every layer is environment-only. Nothing is written to the repository's or your git config,
//! and the app's own pushes, made in its own environment, are untouched by it. A config write
//! made from a task's worktree does reach your checkout, since worktrees share the repository's
//! config, so the app checks where it pushes before every push and confirms the push on GitHub
//! (`qm_core::delivery::push_guard`).

use std::collections::BTreeMap;

/// Where a crewmate's pushed URL goes instead.
pub const PUSH_FENCE_PREFIX: &str = "/dev/null/pushes go through pr_open/";

/// The URL prefixes rewritten for a push, as `pushInsteadOf` matches them (literal prefixes; the
/// longest match wins). The https and ssh schemes go whole. The scp-like forms have no scheme to
/// match, so only GitHub's are named; layer 4 refuses every other one.
pub const PUSH_URLS: &[&str] = &[
    "https://",
    "http://",
    "HTTPS://",
    "HTTP://",
    "Https://",
    "Http://",
    "ssh://",
    "SSH://",
    "git+ssh://",
    "ssh+git://",
    "git://",
    "git@github.com:",
    "github.com:",
    "git@ssh.github.com:",
    "ssh.github.com:",
];

/// Variables that would carry a GitHub token into a process.
pub const TOKEN_VARIABLES: [&str; 4] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// The remotes whose push URL is replaced by the fence (layer 3).
pub const FENCED_REMOTES: [&str; 1] = ["origin"];

/// Layer 4: `ssh` that refuses a push (`git-receive-pack`) to any host and passes everything
/// else through. Git runs it as `sh -c "$GIT_SSH_COMMAND \"$@\""`, so the arguments git adds
/// (options, host, remote command) become the inner script's `$@`.
pub const SSH_FENCE_COMMAND: &str = "sh -c 'for a in \"$@\"; do \
case \"$a\" in *git-receive-pack*) \
echo \"fatal: pushes go through pr_open\" >&2; exit 128;; esac; done; \
exec ssh \"$@\"' qm-ssh-fence";

/// Layer 4: the wrapper takes OpenSSH's options (`-p` for a port, `-o`), so git passes them as
/// it would to `ssh`.
pub const SSH_VARIANT: &str = "ssh";

/// The environment-only git config entries of layers 2 and 3, in order.
pub fn git_config() -> Vec<(String, String)> {
    let insteadof = format!("url.{PUSH_FENCE_PREFIX}.pushInsteadOf");
    let mut out: Vec<(String, String)> = PUSH_URLS
        .iter()
        .map(|url| (insteadof.clone(), (*url).to_owned()))
        .collect();
    for remote in FENCED_REMOTES {
        out.push((
            format!("remote.{remote}.pushurl"),
            format!("{PUSH_FENCE_PREFIX}{remote}"),
        ));
    }
    out
}

/// Adds the fence (layers 1–4) to a crewmate's environment additions; `gh_config_dir` is the
/// task's empty `gh` folder. Any token variable already there is removed.
pub fn apply(env: &mut BTreeMap<String, String>, gh_config_dir: &str) {
    for key in TOKEN_VARIABLES {
        env.remove(key);
    }
    env.insert("GH_CONFIG_DIR".to_owned(), gh_config_dir.to_owned());
    let config = git_config();
    env.insert("GIT_CONFIG_COUNT".to_owned(), config.len().to_string());
    for (i, (key, value)) in config.into_iter().enumerate() {
        env.insert(format!("GIT_CONFIG_KEY_{i}"), key);
        env.insert(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    env.insert("GIT_SSH_COMMAND".to_owned(), SSH_FENCE_COMMAND.to_owned());
    env.insert("GIT_SSH_VARIANT".to_owned(), SSH_VARIANT.to_owned());
    env.insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
}

/// Checks a crewmate's environment additions against the fence: `GH_CONFIG_DIR` is set, no token
/// variable is present, every push URL prefix and every fenced remote has its entry in the
/// environment-only git config, ssh goes through the fence, and git passes ssh options to it as
/// to `ssh`.
pub fn check(env: &BTreeMap<String, String>) -> Result<(), String> {
    if env.get("GH_CONFIG_DIR").is_none_or(|v| v.trim().is_empty()) {
        return Err("GH_CONFIG_DIR isn't set, so gh could use the user's login".to_owned());
    }
    if let Some(key) = TOKEN_VARIABLES.iter().find(|k| env.contains_key(**k)) {
        return Err(format!(
            "{key} is set; a crewmate must never carry a GitHub token"
        ));
    }
    let count: usize = env
        .get("GIT_CONFIG_COUNT")
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let has = |key: &str, value: &str| {
        (0..count).any(|i| {
            env.get(&format!("GIT_CONFIG_KEY_{i}")).map(String::as_str) == Some(key)
                && env
                    .get(&format!("GIT_CONFIG_VALUE_{i}"))
                    .map(String::as_str)
                    == Some(value)
        })
    };
    for (key, value) in git_config() {
        if !has(&key, &value) {
            return Err(format!("the fence misses {key} = {value}"));
        }
    }
    if env.get("GIT_SSH_COMMAND").map(String::as_str) != Some(SSH_FENCE_COMMAND) {
        return Err("ssh pushes aren't fenced (GIT_SSH_COMMAND)".to_owned());
    }
    if env.get("GIT_SSH_VARIANT").map(String::as_str) != Some(SSH_VARIANT) {
        return Err(
            "GIT_SSH_VARIANT isn't ssh, so git would refuse ssh URLs with a port, fetches included"
                .to_owned(),
        );
    }
    Ok(())
}
