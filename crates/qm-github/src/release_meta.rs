//! What GitHub shows after a release tag is pushed: the tag, the GitHub release for it and the
//! workflow runs on its commit. Metadata only.
//!
//! Every read the app makes for this is one of the closed [`ReleaseEndpoint`]s, whose
//! [`ReleaseEndpoint::path`] is the only place a path is built (names percent-encoded, lists with
//! `per_page`). Nothing here follows a URL from an answer, and nothing downloads: the answer types
//! keep no URL at all (no `browser_download_url`, `zipball_url`, `tarball_url`, logs or artifacts),
//! no release notes and no digests. Every answer is read with the
//! `application/vnd.github+json` media type and refused past [`MAX_BODY`].
//!
//! A 404 or 403 here means the login can't see that thing (or it isn't there): the callers report
//! it as unavailable, never as "no workflows" or "failed".

use crate::client::encode_query;
use crate::error::{GithubError, RateState};
use crate::pulls::parse_tag_ref;
use crate::repo::RepoRef;
use crate::{GithubClient, TagRef, client};
use qm_types::BoxFuture;
use qm_types::ids::CommitSha;
use serde::Deserialize;

/// The largest answer read; a larger one is refused unread.
pub const MAX_BODY: usize = 1 << 20;
/// How many of the newest releases are read to find a draft for the tag.
pub const RELEASES_PAGE: u32 = 10;
/// How many workflows are read (a repository rarely has more).
pub const WORKFLOWS_PAGE: u32 = 100;
/// How many runs on the commit are read, newest first.
pub const RUNS_PAGE: u32 = 20;
/// The media type every read asks for.
pub const ACCEPT: &str = "application/vnd.github+json";

/// The only reads the release observer makes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReleaseEndpoint {
    /// `GET /repos/{o}/{r}/git/ref/tags/{tag}`: exactly that tag's ref.
    TagRef { tag: String },
    /// `GET /repos/{o}/{r}/releases/tags/{tag}`: the published release for the tag (GitHub
    /// never shows a draft here).
    ReleaseByTag { tag: String },
    /// `GET /repos/{o}/{r}/releases?per_page=10`: the newest releases, drafts included when the
    /// login can push.
    Releases,
    /// `GET /repos/{o}/{r}/actions/workflows?per_page=100`: the workflows set up.
    Workflows,
    /// `GET /repos/{o}/{r}/actions/runs?head_sha={sha}&exclude_pull_requests=true&per_page=20`:
    /// the runs on the commit.
    Runs { head_sha: CommitSha },
}

impl ReleaseEndpoint {
    /// The API path with its query: the only way a path is built for these reads.
    pub fn path(&self, repo: &RepoRef) -> String {
        let base = repo.path();
        match self {
            Self::TagRef { tag } => {
                format!("{base}/git/ref/tags/{}", client::encode_path(tag))
            }
            Self::ReleaseByTag { tag } => {
                format!("{base}/releases/tags/{}", encode_query(tag))
            }
            Self::Releases => format!("{base}/releases?per_page={RELEASES_PAGE}"),
            Self::Workflows => format!("{base}/actions/workflows?per_page={WORKFLOWS_PAGE}"),
            Self::Runs { head_sha } => format!(
                "{base}/actions/runs?head_sha={}&exclude_pull_requests=true&per_page={RUNS_PAGE}",
                encode_query(head_sha.as_str())
            ),
        }
    }

    /// What the read is about, for an error ("octo/widget's workflow runs").
    fn what(&self, repo: &RepoRef) -> String {
        match self {
            Self::TagRef { tag } => format!("{repo}'s tag {tag}"),
            Self::ReleaseByTag { tag } => format!("{repo}'s release for {tag}"),
            Self::Releases => format!("{repo}'s releases"),
            Self::Workflows => format!("{repo}'s workflows"),
            Self::Runs { .. } => format!("{repo}'s workflow runs"),
        }
    }
}

/// One answer: its status and body (at most [`MAX_BODY`] bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetaResponse {
    pub status: u16,
    pub body: String,
}

impl MetaResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, GithubError> {
        serde_json::from_str(&self.body).map_err(|e| GithubError::Decode(e.to_string()))
    }

    fn error(&self, what: &str, rate: RateState) -> GithubError {
        client::Response {
            status: self.status,
            body: self.body.clone(),
        }
        .error(what, rate)
    }
}

/// Where the reads go: [`GithubClient`] in the app, a fake in tests.
pub trait ReleaseReads: Send + Sync {
    /// One read of `endpoint` in `repo`. Any status comes back; only a call that got no answer
    /// (or an answer past [`MAX_BODY`]) is an error.
    fn read<'a>(
        &'a self,
        repo: &'a RepoRef,
        endpoint: &'a ReleaseEndpoint,
    ) -> BoxFuture<'a, Result<MetaResponse, GithubError>>;

    /// The rate limit as the last answer reported it.
    fn rate(&self) -> RateState {
        RateState::default()
    }
}

impl ReleaseReads for GithubClient {
    fn read<'a>(
        &'a self,
        repo: &'a RepoRef,
        endpoint: &'a ReleaseEndpoint,
    ) -> BoxFuture<'a, Result<MetaResponse, GithubError>> {
        Box::pin(async move {
            let r = self
                .get_capped(&endpoint.path(repo), ACCEPT, MAX_BODY)
                .await?;
            Ok(MetaResponse::new(r.status, r.body))
        })
    }

    fn rate(&self) -> RateState {
        GithubClient::rate(self)
    }
}

/// An asset of a release, as metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AssetMeta {
    pub name: String,
    pub size: u64,
    /// GitHub's `state` is `uploaded`; anything else (`open`) is still uploading.
    pub uploaded: bool,
}

/// A GitHub release, as metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReleaseMeta {
    pub name: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<AssetMeta>,
}

/// What GitHub shows as the release for a tag.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReleaseRead {
    /// No release for the tag (none published, and no draft among the newest).
    None,
    /// A draft: not published, so nothing is offered for download yet.
    Draft(ReleaseMeta),
    Published(ReleaseMeta),
}

/// The workflows set up in a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkflowsMeta {
    /// GitHub's `total_count`: 0 means no workflow is set up.
    pub total: u64,
    /// How many of those read are `active`.
    pub active: u64,
}

/// One workflow run, as metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunMeta {
    pub name: String,
    pub event: String,
    /// `queued`, `in_progress`, `completed`, …
    pub status: String,
    /// Once completed: `success`, `failure`, `cancelled`, …
    pub conclusion: Option<String>,
    /// The branch or tag the run is for (a tag push's runs name the tag).
    pub head_branch: Option<String>,
    pub head_sha: String,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

impl RawRelease {
    fn into_meta(self) -> ReleaseMeta {
        ReleaseMeta {
            name: self.name.filter(|n| !n.trim().is_empty()),
            draft: self.draft,
            prerelease: self.prerelease,
            assets: self
                .assets
                .into_iter()
                .map(|a| AssetMeta {
                    uploaded: a.state == "uploaded",
                    name: a.name,
                    size: a.size,
                })
                .collect(),
        }
    }
}

/// What `refs/tags/<tag>` points at; `None` when GitHub shows no such tag.
pub async fn read_tag(
    reads: &dyn ReleaseReads,
    repo: &RepoRef,
    tag: &str,
) -> Result<Option<TagRef>, GithubError> {
    let endpoint = ReleaseEndpoint::TagRef {
        tag: tag.to_owned(),
    };
    let response = reads.read(repo, &endpoint).await?;
    if response.status == 404 {
        return Ok(None);
    }
    if !response.ok() {
        return Err(response.error(&endpoint.what(repo), reads.rate()));
    }
    parse_tag_ref(&response.body, tag, &endpoint.what(repo)).map(Some)
}

/// The GitHub release for `tag`: the published one, else a draft among the newest
/// [`RELEASES_PAGE`] releases, else none.
pub async fn read_release(
    reads: &dyn ReleaseReads,
    repo: &RepoRef,
    tag: &str,
) -> Result<ReleaseRead, GithubError> {
    let endpoint = ReleaseEndpoint::ReleaseByTag {
        tag: tag.to_owned(),
    };
    let response = reads.read(repo, &endpoint).await?;
    if response.ok() {
        let raw: RawRelease = response.json()?;
        if raw.tag_name != tag {
            return Err(GithubError::Decode(format!(
                "GitHub answered for {} with a release for another tag",
                endpoint.what(repo)
            )));
        }
        let meta = raw.into_meta();
        return Ok(if meta.draft {
            ReleaseRead::Draft(meta)
        } else {
            ReleaseRead::Published(meta)
        });
    }
    if response.status != 404 {
        return Err(response.error(&endpoint.what(repo), reads.rate()));
    }
    let list = ReleaseEndpoint::Releases;
    let response = reads.read(repo, &list).await?;
    if !response.ok() {
        return Err(response.error(&list.what(repo), reads.rate()));
    }
    let raw: Vec<RawRelease> = response.json()?;
    Ok(
        match raw
            .into_iter()
            .find(|r| r.tag_name == tag)
            .map(RawRelease::into_meta)
        {
            Some(meta) if meta.draft => ReleaseRead::Draft(meta),
            Some(meta) => ReleaseRead::Published(meta),
            None => ReleaseRead::None,
        },
    )
}

/// The workflows set up in `repo`.
pub async fn read_workflows(
    reads: &dyn ReleaseReads,
    repo: &RepoRef,
) -> Result<WorkflowsMeta, GithubError> {
    #[derive(Deserialize)]
    struct RawWorkflow {
        #[serde(default)]
        state: String,
    }
    #[derive(Deserialize)]
    struct Raw {
        total_count: u64,
        #[serde(default)]
        workflows: Vec<RawWorkflow>,
    }
    let endpoint = ReleaseEndpoint::Workflows;
    let response = reads.read(repo, &endpoint).await?;
    if !response.ok() {
        return Err(response.error(&endpoint.what(repo), reads.rate()));
    }
    let raw: Raw = response.json()?;
    let active = raw.workflows.iter().filter(|w| w.state == "active").count();
    Ok(WorkflowsMeta {
        total: raw.total_count,
        active: u64::try_from(active).unwrap_or(u64::MAX),
    })
}

/// The newest [`RUNS_PAGE`] workflow runs on `commit`.
pub async fn read_runs(
    reads: &dyn ReleaseReads,
    repo: &RepoRef,
    commit: &CommitSha,
) -> Result<Vec<RunMeta>, GithubError> {
    #[derive(Deserialize)]
    struct RawRun {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        event: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        conclusion: Option<String>,
        #[serde(default)]
        head_branch: Option<String>,
        head_sha: String,
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        workflow_runs: Vec<RawRun>,
    }
    let endpoint = ReleaseEndpoint::Runs {
        head_sha: commit.clone(),
    };
    let response = reads.read(repo, &endpoint).await?;
    if !response.ok() {
        return Err(response.error(&endpoint.what(repo), reads.rate()));
    }
    let raw: Raw = response.json()?;
    Ok(raw
        .workflow_runs
        .into_iter()
        .map(|r| RunMeta {
            name: r.name.unwrap_or_else(|| "a workflow".to_owned()),
            event: r.event,
            status: r.status.unwrap_or_else(|| "unknown".to_owned()),
            conclusion: r.conclusion,
            head_branch: r.head_branch,
            head_sha: r.head_sha,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Answers by path; records every path asked.
    #[derive(Default)]
    struct Fake {
        answers: HashMap<String, MetaResponse>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(mut self, endpoint: &ReleaseEndpoint, status: u16, body: &str) -> Self {
            self.answers
                .insert(endpoint.path(&repo()), MetaResponse::new(status, body));
            self
        }
    }

    impl ReleaseReads for Fake {
        fn read<'a>(
            &'a self,
            repo: &'a RepoRef,
            endpoint: &'a ReleaseEndpoint,
        ) -> BoxFuture<'a, Result<MetaResponse, GithubError>> {
            let path = endpoint.path(repo);
            self.asked.lock().expect("lock").push(path.clone());
            let answer = self
                .answers
                .get(&path)
                .cloned()
                .unwrap_or_else(|| MetaResponse::new(404, r#"{"message":"Not Found"}"#));
            Box::pin(async move { Ok(answer) })
        }
    }

    fn repo() -> RepoRef {
        RepoRef::parse("octo/widget").expect("repo")
    }

    fn sha() -> CommitSha {
        "8a82a2ff738ee593deefddbb2686f7d40c26a204"
            .parse()
            .expect("sha")
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    /// Trimmed from a real `releases/tags/{tag}` answer, download and archive URLs included.
    const RELEASE: &str = r#"{
        "tag_name": "v1.0.0", "name": "Widget 1.0", "draft": false, "prerelease": false,
        "body": "notes", "zipball_url": "https://api.github.com/repos/octo/widget/zipball/v1.0.0",
        "tarball_url": "https://api.github.com/repos/octo/widget/tarball/v1.0.0",
        "html_url": "https://github.com/octo/widget/releases/tag/v1.0.0",
        "assets": [
            {"name": "widget-aarch64-apple-darwin.tar.xz", "size": 4096, "state": "uploaded",
             "browser_download_url": "https://github.com/octo/widget/releases/download/v1.0.0/widget.tar.xz",
             "digest": "sha256:abc", "download_count": 3},
            {"name": "widget.sha256", "size": 90, "state": "open",
             "browser_download_url": "https://github.com/octo/widget/releases/download/v1.0.0/widget.sha256"}
        ]
    }"#;

    #[test]
    fn every_path_is_closed_encoded_and_never_a_download() {
        let tag = "v1.0.0+build/1 ?#";
        let endpoints = [
            ReleaseEndpoint::TagRef { tag: tag.into() },
            ReleaseEndpoint::ReleaseByTag { tag: tag.into() },
            ReleaseEndpoint::Releases,
            ReleaseEndpoint::Workflows,
            ReleaseEndpoint::Runs { head_sha: sha() },
        ];
        let paths: Vec<String> = endpoints.iter().map(|e| e.path(&repo())).collect();
        assert_eq!(
            paths,
            vec![
                "/repos/octo/widget/git/ref/tags/v1.0.0%2Bbuild/1%20%3F%23",
                "/repos/octo/widget/releases/tags/v1.0.0%2Bbuild%2F1%20%3F%23",
                "/repos/octo/widget/releases?per_page=10",
                "/repos/octo/widget/actions/workflows?per_page=100",
                "/repos/octo/widget/actions/runs?head_sha=8a82a2ff738ee593deefddbb2686f7d40c26a204&exclude_pull_requests=true&per_page=20",
            ]
        );
        for path in &paths {
            for banned in [
                "/assets",
                "zipball",
                "tarball",
                "artifacts",
                "/logs",
                "download",
                "://",
            ] {
                assert!(!path.contains(banned), "{path} has {banned}");
            }
            assert!(path.starts_with("/repos/octo/widget/"), "{path}");
        }
    }

    #[test]
    fn a_published_release_keeps_metadata_only() {
        let fake = Fake::default().with(
            &ReleaseEndpoint::ReleaseByTag {
                tag: "v1.0.0".into(),
            },
            200,
            RELEASE,
        );
        let read = block_on(read_release(&fake, &repo(), "v1.0.0")).expect("read");
        let ReleaseRead::Published(meta) = read else {
            panic!("{read:?}");
        };
        assert_eq!(meta.name.as_deref(), Some("Widget 1.0"));
        assert_eq!(meta.assets.len(), 2);
        assert!(meta.assets[0].uploaded);
        assert!(!meta.assets[1].uploaded, "open is still uploading");
        let shown = format!("{meta:?}");
        for kept_out in ["https://", "notes", "sha256:abc"] {
            assert!(!shown.contains(kept_out), "{shown}");
        }
        assert_eq!(fake.asked.lock().expect("lock").len(), 1);
    }

    #[test]
    fn a_draft_is_found_in_the_list_and_none_is_none() {
        let list = r#"[{"tag_name": "v0.9.0", "draft": false, "assets": []},
                       {"tag_name": "v1.0.0", "name": "", "draft": true, "assets": []}]"#;
        let fake = Fake::default().with(&ReleaseEndpoint::Releases, 200, list);
        let read = block_on(read_release(&fake, &repo(), "v1.0.0")).expect("read");
        assert!(
            matches!(read, ReleaseRead::Draft(ReleaseMeta { name: None, .. })),
            "{read:?}"
        );
        let read = block_on(read_release(&fake, &repo(), "v2.0.0")).expect("read");
        assert_eq!(read, ReleaseRead::None);
    }

    #[test]
    fn a_refusal_is_an_error_never_an_empty_answer() {
        let denied = r#"{"message":"Resource not accessible by personal access token"}"#;
        let fake = Fake::default()
            .with(&ReleaseEndpoint::Workflows, 403, denied)
            .with(&ReleaseEndpoint::Runs { head_sha: sha() }, 403, denied)
            .with(&ReleaseEndpoint::Releases, 403, denied);
        let e = block_on(read_workflows(&fake, &repo())).expect_err("403");
        assert!(matches!(e, GithubError::Http { status: 403, .. }), "{e:?}");
        assert!(!e.is_transient());
        let e = block_on(read_runs(&fake, &repo(), &sha())).expect_err("403");
        assert!(matches!(e, GithubError::Http { status: 403, .. }), "{e:?}");
        // The release by tag is 404, then the list is refused: an error, not "none".
        let e = block_on(read_release(&fake, &repo(), "v1.0.0")).expect_err("403");
        assert!(matches!(e, GithubError::Http { status: 403, .. }), "{e:?}");
        // A 404 on workflows is the login not seeing them, not "no workflows".
        let e = block_on(read_workflows(&Fake::default(), &repo())).expect_err("404");
        assert!(matches!(e, GithubError::NotFound(_)), "{e:?}");
    }

    #[test]
    fn workflows_and_runs_keep_their_counts_and_names() {
        let workflows = r#"{"total_count": 3, "workflows": [
            {"name": "ci", "state": "active", "path": ".github/workflows/ci.yml"},
            {"name": "Release", "state": "disabled_manually"},
            {"name": "notes", "state": "active"}]}"#;
        let runs = r#"{"total_count": 2, "workflow_runs": [
            {"name": "Release", "event": "push", "status": "in_progress", "conclusion": null,
             "head_branch": "v1.0.0", "head_sha": "8a82a2ff738ee593deefddbb2686f7d40c26a204",
             "logs_url": "https://api.github.com/x/logs", "artifacts_url": "https://api.github.com/x/artifacts"},
            {"name": "ci", "event": "push", "status": "completed", "conclusion": "success",
             "head_branch": "main", "head_sha": "8a82a2ff738ee593deefddbb2686f7d40c26a204"}]}"#;
        let fake = Fake::default()
            .with(&ReleaseEndpoint::Workflows, 200, workflows)
            .with(&ReleaseEndpoint::Runs { head_sha: sha() }, 200, runs);
        let w = block_on(read_workflows(&fake, &repo())).expect("workflows");
        assert_eq!((w.total, w.active), (3, 2));
        let r = block_on(read_runs(&fake, &repo(), &sha())).expect("runs");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].head_branch.as_deref(), Some("v1.0.0"));
        assert_eq!(r[0].conclusion, None);
        assert!(!format!("{r:?}").contains("https://"));
        let none = r#"{"total_count": 0, "workflows": []}"#;
        let fake = Fake::default().with(&ReleaseEndpoint::Workflows, 200, none);
        assert_eq!(
            block_on(read_workflows(&fake, &repo())).expect("w").total,
            0
        );
    }

    #[test]
    fn a_missing_tag_is_none_and_another_ref_is_refused() {
        assert_eq!(
            block_on(read_tag(&Fake::default(), &repo(), "v1")).expect("read"),
            None
        );
        let other = r#"{"ref": "refs/tags/v1.0.0-rc", "object": {"type": "tag", "sha": "9ff55deafae68869e49470ac30505050b2df1e53"}}"#;
        let fake = Fake::default().with(
            &ReleaseEndpoint::TagRef {
                tag: "v1.0.0".into(),
            },
            200,
            other,
        );
        assert!(block_on(read_tag(&fake, &repo(), "v1.0.0")).is_err());
    }
}
