//! Pull requests: find, create, update, read, merge at an exact commit, close; and reading and
//! deleting a branch.

use crate::client::{GithubClient, Method, encode_path, encode_query};
use crate::error::GithubError;
use crate::repo::RepoRef;
use qm_types::domain::MergeMethod;
use qm_types::ids::CommitSha;
use serde::Deserialize;

/// A pull request's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
}

/// A pull request as GitHub reports it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    pub merged: bool,
    /// Only when merged.
    pub merge_commit: Option<CommitSha>,
    pub head_sha: CommitSha,
    pub head_ref: String,
    /// The repository the head branch lives in; `None` when it was deleted.
    pub head_repo: Option<RepoRef>,
    pub base_ref: String,
    pub title: String,
    /// `None` while GitHub is still computing it.
    pub mergeable: Option<bool>,
    pub mergeable_state: Option<String>,
    pub additions: u64,
    pub deletions: u64,
}

/// What a merge request came back with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeResult {
    /// 200 with `merged: true`.
    Merged { merge_commit: CommitSha },
    /// 409: the head isn't the SHA sent; nothing was merged.
    HeadMoved { message: String },
    /// 405 or 422: GitHub won't merge it as it is, with its reason.
    NotMergeable { message: String },
    /// No clear answer (a timeout, a connection error, a 5xx, anything else): read it back.
    Unknown { message: String },
}

/// A repository's facts the delivery probe needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub default_branch: String,
    pub can_push: bool,
    pub archived: bool,
}

#[derive(Deserialize)]
struct RawOwner {
    login: String,
}

#[derive(Deserialize)]
struct RawRepo {
    name: String,
    owner: RawOwner,
}

#[derive(Deserialize)]
struct RawRef {
    sha: String,
    #[serde(rename = "ref")]
    name: String,
    repo: Option<RawRepo>,
}

#[derive(Deserialize)]
struct RawPull {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    head: RawRef,
    base: RawRef,
    title: String,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    mergeable_state: Option<String>,
    #[serde(default)]
    additions: Option<u64>,
    #[serde(default)]
    deletions: Option<u64>,
}

fn sha(s: &str) -> Result<CommitSha, GithubError> {
    s.parse().map_err(|e| GithubError::Decode(format!("{e}")))
}

impl RawPull {
    fn into_pull(self) -> Result<PullRequest, GithubError> {
        let merged = self.merged.unwrap_or(self.merged_at.is_some());
        Ok(PullRequest {
            number: self.number,
            url: self.html_url,
            state: if self.state == "open" {
                PrState::Open
            } else {
                PrState::Closed
            },
            merged,
            merge_commit: if merged {
                self.merge_commit_sha.as_deref().map(sha).transpose()?
            } else {
                None
            },
            head_sha: sha(&self.head.sha)?,
            head_ref: self.head.name,
            head_repo: self.head.repo.map(|r| RepoRef {
                owner: r.owner.login,
                name: r.name,
            }),
            base_ref: self.base.name,
            title: self.title,
            mergeable: self.mergeable,
            mergeable_state: self.mergeable_state,
            additions: self.additions.unwrap_or(0),
            deletions: self.deletions.unwrap_or(0),
        })
    }
}

fn pr_what(r: &RepoRef, number: u64) -> String {
    format!("{r}'s pull request #{number}")
}

impl GithubClient {
    /// The repository's default branch, whether the login can push, and whether it's archived.
    pub async fn repo(&self, r: &RepoRef) -> Result<RepoInfo, GithubError> {
        #[derive(Deserialize)]
        struct Permissions {
            #[serde(default)]
            push: bool,
        }
        #[derive(Deserialize)]
        struct Raw {
            default_branch: String,
            #[serde(default)]
            archived: bool,
            #[serde(default)]
            permissions: Option<Permissions>,
        }
        let raw: Raw = self.get_json(&r.path(), &r.to_string()).await?;
        Ok(RepoInfo {
            default_branch: raw.default_branch,
            can_push: raw.permissions.is_some_and(|p| p.push),
            archived: raw.archived,
        })
    }

    /// The open pull request whose head is `head_branch` in the repository itself (never a
    /// fork's branch of the same name).
    pub async fn find_open_pr(
        &self,
        r: &RepoRef,
        head_branch: &str,
    ) -> Result<Option<PullRequest>, GithubError> {
        let head = encode_query(&format!("{}:{head_branch}", r.owner));
        let path = format!("{}/pulls?state=open&head={head}&per_page=10", r.path());
        let raw: Vec<RawPull> = self
            .get_json(&path, &format!("{r}'s pull requests"))
            .await?;
        for pull in raw {
            let pull = pull.into_pull()?;
            if pull.head_ref == head_branch && pull.head_repo.as_ref() == Some(r) {
                return Ok(Some(pull));
            }
        }
        Ok(None)
    }

    pub async fn create_pr(
        &self,
        r: &RepoRef,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest, GithubError> {
        let request =
            serde_json::json!({ "title": title, "body": body, "head": head, "base": base });
        let response = self
            .send(Method::Post, &format!("{}/pulls", r.path()), Some(&request))
            .await?;
        if !response.ok() {
            return Err(response.error(&format!("{r}'s pull requests"), self.rate()));
        }
        response.json::<RawPull>()?.into_pull()
    }

    pub async fn update_pr(
        &self,
        r: &RepoRef,
        number: u64,
        title: &str,
        body: &str,
    ) -> Result<PullRequest, GithubError> {
        let request = serde_json::json!({ "title": title, "body": body });
        let response = self
            .send(
                Method::Patch,
                &format!("{}/pulls/{number}", r.path()),
                Some(&request),
            )
            .await?;
        if !response.ok() {
            return Err(response.error(&pr_what(r, number), self.rate()));
        }
        response.json::<RawPull>()?.into_pull()
    }

    /// The pull request as it is on GitHub now.
    pub async fn pull(&self, r: &RepoRef, number: u64) -> Result<PullRequest, GithubError> {
        let raw: RawPull = self
            .get_json(&format!("{}/pulls/{number}", r.path()), &pr_what(r, number))
            .await?;
        raw.into_pull()
    }

    /// Merges pull request `number` only if its head is exactly `sha`: GitHub's merge endpoint with
    /// `sha` and `merge_method`, and no commit title or message, so the repository's own settings
    /// apply. Never retried here.
    pub async fn merge_pr(
        &self,
        r: &RepoRef,
        number: u64,
        sha: &CommitSha,
        method: MergeMethod,
    ) -> MergeResult {
        // The request always carries the approved SHA as its precondition.
        let request = serde_json::json!({ "sha": sha.as_str(), "merge_method": method.as_str() });
        let response = match self
            .send(
                Method::Put,
                &format!("{}/pulls/{number}/merge", r.path()),
                Some(&request),
            )
            .await
        {
            Ok(response) => response,
            Err(e) => {
                return MergeResult::Unknown { message: e.plain() };
            }
        };
        match response.status {
            200 => {
                #[derive(Deserialize)]
                struct Raw {
                    #[serde(default)]
                    merged: bool,
                    #[serde(default)]
                    sha: Option<String>,
                }
                match response.json::<Raw>() {
                    Ok(Raw {
                        merged: true,
                        sha: Some(commit),
                    }) => match commit.parse() {
                        Ok(merge_commit) => MergeResult::Merged { merge_commit },
                        Err(_) => MergeResult::Unknown {
                            message: "GitHub said merged but named no commit".to_owned(),
                        },
                    },
                    _ => MergeResult::Unknown {
                        message: response.message(),
                    },
                }
            }
            409 => MergeResult::HeadMoved {
                message: response.message(),
            },
            405 | 422 => MergeResult::NotMergeable {
                message: response.message(),
            },
            _ => MergeResult::Unknown {
                message: response.message(),
            },
        }
    }

    /// Closes a pull request with a comment saying why.
    pub async fn close_pr(
        &self,
        r: &RepoRef,
        number: u64,
        comment: &str,
    ) -> Result<(), GithubError> {
        let request = serde_json::json!({ "body": comment });
        let response = self
            .send(
                Method::Post,
                &format!("{}/issues/{number}/comments", r.path()),
                Some(&request),
            )
            .await?;
        if !response.ok() {
            return Err(response.error(&pr_what(r, number), self.rate()));
        }
        let request = serde_json::json!({ "state": "closed" });
        let response = self
            .send(
                Method::Patch,
                &format!("{}/pulls/{number}", r.path()),
                Some(&request),
            )
            .await?;
        if !response.ok() {
            return Err(response.error(&pr_what(r, number), self.rate()));
        }
        Ok(())
    }

    /// The commit `refs/heads/<branch>` is at on GitHub now; `None` when there's no such branch.
    /// `GET /repos/{o}/{r}/git/ref/heads/{branch}` (the singular `ref`, which matches the name
    /// exactly, never a prefix), and the answer must name exactly that ref and a commit. How the
    /// app confirms a push of its own reached GitHub before recording it.
    pub async fn branch_head(
        &self,
        r: &RepoRef,
        branch: &str,
    ) -> Result<Option<CommitSha>, GithubError> {
        #[derive(Deserialize)]
        struct RawObject {
            sha: String,
            #[serde(rename = "type")]
            kind: String,
        }
        #[derive(Deserialize)]
        struct RawGitRef {
            #[serde(rename = "ref")]
            name: String,
            object: RawObject,
        }
        let what = format!("{r}'s branch {branch}");
        let response = self
            .send(
                Method::Get,
                &format!("{}/git/ref/heads/{}", r.path(), encode_path(branch)),
                None,
            )
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        if !response.ok() {
            return Err(response.error(&what, self.rate()));
        }
        let raw: RawGitRef = response.json()?;
        if raw.name != format!("refs/heads/{branch}") || raw.object.kind != "commit" {
            return Err(GithubError::Decode(format!(
                "GitHub answered for {what} with {} ({}), not that branch's commit",
                raw.name, raw.object.kind
            )));
        }
        sha(&raw.object.sha).map(Some)
    }

    /// Deletes a branch on GitHub; a branch already gone is fine.
    pub async fn delete_branch(&self, r: &RepoRef, branch: &str) -> Result<(), GithubError> {
        let response = self
            .send(
                Method::Delete,
                &format!("{}/git/refs/heads/{}", r.path(), encode_path(branch)),
                None,
            )
            .await?;
        if response.ok() || response.status == 404 || response.status == 422 {
            Ok(())
        } else {
            Err(response.error(&format!("{r}'s branch {branch}"), self.rate()))
        }
    }
}
