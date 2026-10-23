//! Release tags: the app creates and pushes exactly one tag, create-only, after your approval.
//!
//! - [`Repo::check_tag_name`] asks git whether `refs/tags/<tag>` is a valid ref name.
//! - [`Repo::write_tag_object`] writes an annotated tag object into the object store, and reads it
//!   back to check it holds exactly what was meant. No local ref is created: nothing in your
//!   checkout changes.
//! - [`Repo::push_tag`] pushes that object (or, for a lightweight tag, the commit) to
//!   `refs/tags/<tag>` with a lease that requires the tag to be absent on the remote, so an
//!   existing tag is never moved or overwritten. It shares [`Repo::push`]'s guards: the write lock,
//!   the exact checked URL, the app's own transport and credentials, hooks off.
//! - [`Repo::remote_tag`] reads what the remote's tag points at now.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use qm_types::ids::CommitSha;

use crate::error::GitError;
use crate::land::PushCredential;
use crate::network::{Credentials, RepoTransportSetting};
use crate::repo::{Repo, parse_sha, unexpected};

/// One tag push.
#[derive(Clone, Debug)]
pub struct TagPushSpec<'a> {
    pub remote: &'a str,
    /// The one URL the caller checked `remote` pushes to; see [`crate::PushSpec::url`].
    pub url: &'a str,
    pub tag: &'a str,
    /// What `refs/tags/<tag>` should point at: the tag object for an annotated tag, the commit
    /// for a lightweight one. It must already be in the object store.
    pub object: &'a str,
    pub credential: PushCredential,
}

/// What a tag push did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagPushOutcome {
    /// Git said it pushed, or that the remote already had exactly this object. Confirm on the
    /// remote before calling it published.
    Pushed,
    /// The remote already has the tag, pointing at `remote_now` (`None` if it couldn't be read
    /// back): nothing was overwritten.
    Exists { remote_now: Option<String> },
    /// The remote refused it for another reason, with git's words.
    Rejected { message: String },
    /// `remote` no longer pushes to exactly the checked URL: nothing was pushed.
    Redirected { urls: Vec<String> },
    /// Repository config changes how a push travels: nothing was pushed.
    RepoConfig { settings: Vec<RepoTransportSetting> },
}

/// Longest message accepted for a tag object.
const MAX_MESSAGE: usize = 8_000;

fn scratch_file() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("qm-release-tag-{}-{nanos:x}", std::process::id()))
}

impl Repo {
    /// Whether git accepts `refs/tags/<tag>` as a ref name.
    pub fn check_tag_name(&self, tag: &str) -> Result<bool, GitError> {
        let reference = format!("refs/tags/{tag}");
        let out = self
            .git()
            .invoke(self.root(), ["check-ref-format", reference.as_str()])
            .output()?;
        Ok(out.success())
    }

    /// Whether `commit` is in the object store as a commit.
    pub fn has_commit(&self, commit: &CommitSha) -> Result<bool, GitError> {
        Ok(self.resolve_commit(commit.as_str())?.as_ref() == Some(commit))
    }

    /// Whether `ancestor` is reachable from `descendant`.
    pub fn contains_commit(
        &self,
        descendant: &CommitSha,
        ancestor: &CommitSha,
    ) -> Result<bool, GitError> {
        self.is_ancestor(ancestor, descendant)
    }

    /// Fetches `branch` from `remote` into its remote-tracking ref and returns its commit.
    pub fn fetch_remote_branch(&self, remote: &str, branch: &str) -> Result<CommitSha, GitError> {
        self.fetch_branch(remote, branch)
    }

    /// Writes an annotated tag object for `commit` named `tag`, tagged by your committer
    /// identity, and returns its id. The object is read back and must hold exactly the text
    /// written. No ref is created.
    pub fn write_tag_object(
        &self,
        commit: &CommitSha,
        tag: &str,
        message: &str,
    ) -> Result<String, GitError> {
        if message.len() > MAX_MESSAGE || message.contains('\0') {
            return Err(unexpected("tag message", "too long or holds a NUL byte"));
        }
        if !self.has_commit(commit)? {
            return Err(unexpected("tag", "the commit isn't in the repository"));
        }
        let ident = self.run(["var", "GIT_COMMITTER_IDENT"])?;
        let ident = ident.trim();
        if ident.is_empty() || ident.contains('\n') {
            return Err(unexpected("var GIT_COMMITTER_IDENT", "no single identity"));
        }
        let mut body = message.trim_end().to_owned();
        body.push('\n');
        let text = format!(
            "object {}\ntype commit\ntag {tag}\ntagger {ident}\n\n{body}",
            commit.as_str()
        );
        let path = scratch_file();
        let written = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })();
        let hashed = written
            .map_err(|e| unexpected("tag", &format!("couldn't write the tag text: {e}")))
            .and_then(|()| {
                self.run([
                    "hash-object".as_ref(),
                    "-t".as_ref(),
                    "tag".as_ref(),
                    "-w".as_ref(),
                    path.as_os_str(),
                ])
            });
        let _ = std::fs::remove_file(&path);
        let object = parse_sha("hash-object", hashed?.trim())?
            .as_str()
            .to_owned();
        let stored = self.run(["cat-file", "tag", object.as_str()])?;
        if stored != text {
            return Err(unexpected(
                "cat-file tag",
                "the tag object isn't what was written",
            ));
        }
        Ok(object)
    }

    /// What `refs/tags/<tag>` is on `remote` now (the tag object, or the commit for a
    /// lightweight tag), asked over the app's own transport.
    pub fn remote_tag(
        &self,
        remote: &str,
        tag: &str,
        credential: &PushCredential,
    ) -> Result<Option<String>, GitError> {
        self.remote_tag_with(remote, tag, credentials(credential))
    }

    fn remote_tag_with(
        &self,
        remote: &str,
        tag: &str,
        credentials: Credentials<'_>,
    ) -> Result<Option<String>, GitError> {
        let reference = format!("refs/tags/{tag}");
        let out = self
            .network_command(
                remote,
                None,
                credentials,
                ["ls-remote", "--refs", remote, reference.as_str()],
            )?
            .text()?;
        out.lines()
            .find_map(|line| {
                let (sha, name) = line.split_once('\t')?;
                (name == reference)
                    .then(|| parse_sha("ls-remote", sha).map(|s| s.as_str().to_owned()))
            })
            .transpose()
    }

    /// Every tag name on `remote` now (`git ls-remote --tags --refs`), asked over the app's own
    /// transport with your own credential helpers; sorted.
    pub fn remote_tags(&self, remote: &str) -> Result<Vec<String>, GitError> {
        let out = self
            .network_command(
                remote,
                None,
                Credentials::Yours,
                ["ls-remote", "--tags", "--refs", remote],
            )?
            .text()?;
        let mut tags: Vec<String> = out
            .lines()
            .filter_map(|line| line.split_once('\t')?.1.strip_prefix("refs/tags/"))
            .map(str::to_owned)
            .collect();
        tags.sort();
        tags.dedup();
        Ok(tags)
    }

    /// Pushes exactly `spec.object` to `refs/tags/<spec.tag>` on `remote`, create-only: the lease
    /// requires the tag to be absent, so an existing tag is reported
    /// ([`TagPushOutcome::Exists`]) and never moved. Same guards as [`Repo::push`].
    pub fn push_tag(&self, spec: &TagPushSpec<'_>) -> Result<TagPushOutcome, GitError> {
        let _lock = self.write_lock();
        let urls = self.push_urls(spec.remote)?;
        if urls != [spec.url] {
            return Ok(TagPushOutcome::Redirected { urls });
        }
        let credentials = credentials(&spec.credential);
        let object = parse_sha("tag object", spec.object)?;
        let reference = format!("refs/tags/{}", spec.tag);
        let refspec = format!("{}:{reference}", object.as_str());
        // An empty expected value: the ref must not exist on the remote.
        let lease = format!("--force-with-lease={reference}:");
        let args = [
            "push",
            "--porcelain",
            "--no-verify",
            "--no-follow-tags",
            "--no-recurse-submodules",
            "--no-signed",
            "--no-force-if-includes",
            "--no-atomic",
            lease.as_str(),
            spec.remote,
            refspec.as_str(),
        ];
        let invocation = match self.network_command(spec.remote, Some(spec.url), credentials, args)
        {
            Err(GitError::RepoTransportConfig { settings, .. }) => {
                return Ok(TagPushOutcome::RepoConfig { settings });
            }
            other => other?,
        };
        let out = invocation.output()?;
        if out.success() {
            return Ok(TagPushOutcome::Pushed);
        }
        let rejected = out
            .stdout
            .lines()
            .find(|l| l.starts_with('!') && l.contains(&reference));
        match rejected {
            Some(line) if line.contains("stale info") || line.contains("already exists") => {
                match self.remote_tag_with(spec.remote, spec.tag, credentials) {
                    Ok(remote_now) => Ok(TagPushOutcome::Exists { remote_now }),
                    Err(GitError::RepoTransportConfig { settings, .. }) => {
                        Ok(TagPushOutcome::RepoConfig { settings })
                    }
                    Err(_) => Ok(TagPushOutcome::Exists { remote_now: None }),
                }
            }
            Some(line) => Ok(TagPushOutcome::Rejected {
                message: [line.trim(), out.stderr.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("; "),
            }),
            None => Err(GitError::Failed {
                command: format!("push {} {reference}", spec.remote),
                code: out.code,
                stderr: out.stderr,
            }),
        }
    }
}

fn credentials(credential: &PushCredential) -> Credentials<'_> {
    match credential {
        PushCredential::Default => Credentials::Yours,
        PushCredential::GhHelper { gh } => Credentials::Gh(gh),
    }
}
