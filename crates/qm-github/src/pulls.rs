//! Pull requests: find, create, update, read, merge at an exact commit, close; and reading and
//! deleting a branch.

use crate::client::{GithubClient, Method, encode_path, encode_query};
use crate::error::GithubError;
use crate::repo::RepoRef;
use qm_types::ids::CommitSha;
use serde::Deserialize;

/// A pull request's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
}

/// The commit a squash merge makes: the pull request's title and description, as checked right
/// before the merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SquashCommit {
    pub title: String,
    pub message: String,
}

/// What GitHub says about squash-merging into a branch. `None` where it couldn't be read: the
/// merge itself then says no if squash isn't allowed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SquashRules {
    /// The repository's "Allow squash merging" setting.
    pub repo_allows: Option<bool>,
    /// The merge methods the branch's rulesets allow, when a ruleset limits them.
    pub ruleset_methods: Option<Vec<String>>,
}

/// The body of GitHub's merge request: always a squash, at exactly `sha`, with the commit's title
/// and message given, so the repository's defaults never choose them.
pub fn squash_request(sha: &CommitSha, commit: &SquashCommit) -> serde_json::Value {
    serde_json::json!({
        "sha": sha.as_str(),
        "merge_method": "squash",
        "commit_title": commit.title,
        "commit_message": commit.message,
    })
}

/// The merge methods every `pull_request` rule allows (the rules endpoint's answer), or `None`
/// when no rule limits them.
fn ruleset_methods(rules: &serde_json::Value) -> Option<Vec<String>> {
    let mut allowed: Option<Vec<String>> = None;
    for rule in rules.as_array()? {
        if rule.get("type").and_then(serde_json::Value::as_str) != Some("pull_request") {
            continue;
        }
        let Some(methods) = rule
            .pointer("/parameters/allowed_merge_methods")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        let methods: Vec<String> = methods
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        allowed = Some(match allowed {
            None => methods,
            Some(before) => before.into_iter().filter(|m| methods.contains(m)).collect(),
        });
    }
    allowed
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
    /// The description; empty when it has none.
    pub body: String,
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

/// What a tag ref points at on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagRef {
    /// `tag` for an annotated tag's object, `commit` for a lightweight tag.
    pub kind: String,
    /// The object's id, lowercase hex.
    pub sha: String,
}

/// A `git/ref/tags/{tag}` answer, which must name exactly that ref.
pub(crate) fn parse_tag_ref(body: &str, tag: &str, what: &str) -> Result<TagRef, GithubError> {
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
    let raw: RawGitRef =
        serde_json::from_str(body).map_err(|e| GithubError::Decode(e.to_string()))?;
    if raw.name != format!("refs/tags/{tag}")
        || !matches!(raw.object.kind.as_str(), "tag" | "commit")
    {
        return Err(GithubError::Decode(format!(
            "GitHub answered for {what} with {} ({}), not that tag",
            raw.name, raw.object.kind
        )));
    }
    let sha = sha(&raw.object.sha)?;
    Ok(TagRef {
        kind: raw.object.kind,
        sha: sha.as_str().to_owned(),
    })
}

/// A repository's facts the delivery probe needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// GitHub's numeric id: stays the same across renames and transfers.
    pub id: u64,
    /// `owner/name` as GitHub shows it now (after any rename or transfer).
    pub full_name: String,
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
    body: Option<String>,
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
            body: self.body.unwrap_or_default(),
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
            id: u64,
            full_name: String,
            default_branch: String,
            #[serde(default)]
            archived: bool,
            #[serde(default)]
            permissions: Option<Permissions>,
        }
        let raw: Raw = self.get_json(&r.path(), &r.to_string()).await?;
        Ok(RepoInfo {
            id: raw.id,
            full_name: raw.full_name,
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

    /// Whether GitHub lets a pull request into `branch` be squash-merged: the repository's own
    /// setting and the branch's rulesets. What can't be read is `None`.
    pub async fn squash_rules(&self, r: &RepoRef, branch: &str) -> SquashRules {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            allow_squash_merge: Option<bool>,
        }
        let repo_allows = match self.get_json::<Raw>(&r.path(), &r.to_string()).await {
            Ok(raw) => raw.allow_squash_merge,
            Err(_) => None,
        };
        let path = format!("{}/rules/branches/{}", r.path(), encode_path(branch));
        let ruleset_methods = match self.send(Method::Get, &path, None).await {
            Ok(response) if response.ok() => response
                .json::<serde_json::Value>()
                .ok()
                .and_then(|rules| ruleset_methods(&rules)),
            _ => None,
        };
        SquashRules {
            repo_allows,
            ruleset_methods,
        }
    }

    /// Squash-merges pull request `number` only if its head is exactly `sha`, with `commit`'s title
    /// and message ([`squash_request`]). Never retried here.
    pub async fn merge_pr(
        &self,
        r: &RepoRef,
        number: u64,
        sha: &CommitSha,
        commit: &SquashCommit,
    ) -> MergeResult {
        // The request always carries the approved SHA as its precondition.
        let request = squash_request(sha, commit);
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

    /// What `refs/tags/<tag>` points at on GitHub now; `None` when there's no such tag.
    /// `GET /repos/{o}/{r}/git/ref/tags/{tag}`, the singular `ref` (an exact match, never a
    /// prefix); the answer must name exactly that ref. How the app confirms a tag it pushed.
    pub async fn tag_ref(&self, r: &RepoRef, tag: &str) -> Result<Option<TagRef>, GithubError> {
        let what = format!("{r}'s tag {tag}");
        let response = self
            .send(
                Method::Get,
                &format!("{}/git/ref/tags/{}", r.path(), encode_path(tag)),
                None,
            )
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        if !response.ok() {
            return Err(response.error(&what, self.rate()));
        }
        parse_tag_ref(&response.body, tag, &what).map(Some)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merge_is_always_a_squash_with_the_title_and_body() {
        let sha: CommitSha = "0123456789abcdef0123456789abcdef01234567"
            .parse()
            .expect("sha");
        let commit = SquashCommit {
            title: "fix(core): check only a task's own commits".to_owned(),
            message: "## Summary\nThe body.".to_owned(),
        };
        assert_eq!(
            squash_request(&sha, &commit),
            serde_json::json!({
                "sha": "0123456789abcdef0123456789abcdef01234567",
                "merge_method": "squash",
                "commit_title": "fix(core): check only a task's own commits",
                "commit_message": "## Summary\nThe body.",
            })
        );
    }

    #[test]
    fn rulesets_limit_merge_methods_only_through_pull_request_rules() {
        let none = serde_json::json!([{ "type": "required_status_checks", "parameters": {} }]);
        assert_eq!(ruleset_methods(&none), None);
        let rules = serde_json::json!([
            { "type": "pull_request", "parameters": { "allowed_merge_methods": ["merge", "squash"] } },
            { "type": "pull_request", "parameters": { "allowed_merge_methods": ["merge", "rebase"] } },
        ]);
        assert_eq!(ruleset_methods(&rules), Some(vec!["merge".to_owned()]));
    }

    #[test]
    fn a_pull_without_a_body_reads_as_empty() {
        let raw: RawPull = serde_json::from_value(serde_json::json!({
            "number": 3, "html_url": "https://github.com/o/r/pull/3", "state": "open",
            "head": { "sha": "0123456789abcdef0123456789abcdef01234567", "ref": "qm/task-0003",
                      "repo": { "name": "r", "owner": { "login": "o" } } },
            "base": { "sha": "0123456789abcdef0123456789abcdef01234567", "ref": "main" },
            "title": "fix(core): one", "body": null,
        }))
        .expect("a pull");
        assert_eq!(raw.into_pull().expect("pull").body, "");
    }
}
