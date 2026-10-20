//! `github_read` and `github_write`: issues, pull requests, comments, files, labels.

use crate::client::{GithubClient, Method, encode_path, encode_query};
use crate::error::GithubError;
use crate::repo::RepoRef;
use base64::Engine as _;
use qm_types::domain::{GithubReadKind, GithubWriteAction};
use serde::Deserialize;
use std::fmt::Write as _;

/// The newest comments read.
pub const COMMENTS_READ: u64 = 50;
/// The largest file read, in bytes.
pub const FILE_MAX_BYTES: u64 = 200 * 1024;

/// What to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRead {
    pub what: GithubReadKind,
    pub number: Option<u64>,
    pub path: Option<String>,
    pub git_ref: Option<String>,
}

/// What to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubWrite {
    pub action: GithubWriteAction,
    pub number: Option<u64>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Deserialize)]
struct RawUser {
    login: String,
}

#[derive(Deserialize)]
struct RawIssue {
    title: String,
    state: String,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    comments: u64,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct RawComment {
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct RawCreated {
    #[serde(default)]
    html_url: Option<String>,
}

fn missing(what: &str) -> GithubError {
    GithubError::Http {
        status: 422,
        message: format!("{what} is needed for this"),
    }
}

fn author(user: Option<&RawUser>) -> &str {
    user.map_or("someone", |u| u.login.as_str())
}

impl GithubClient {
    /// Reads an issue, a pull request, its comments or a file, as text.
    pub async fn read(&self, r: &RepoRef, read: &GithubRead) -> Result<String, GithubError> {
        match read.what {
            GithubReadKind::Issue | GithubReadKind::PullRequest => {
                let n = read.number.ok_or_else(|| missing("number"))?;
                let (segment, noun) = if read.what == GithubReadKind::Issue {
                    ("issues", "issue")
                } else {
                    ("pulls", "pull request")
                };
                let issue: RawIssue = self
                    .get_json(
                        &format!("{}/{segment}/{n}", r.path()),
                        &format!("{r}'s {noun} #{n}"),
                    )
                    .await?;
                let mut out = format!(
                    "{r}#{n}: {}\nstate: {} · by {}\n",
                    issue.title,
                    issue.state,
                    author(issue.user.as_ref())
                );
                if let Some(url) = &issue.html_url {
                    let _ = writeln!(out, "{url}");
                }
                out.push('\n');
                out.push_str(issue.body.as_deref().unwrap_or("(no description)"));
                Ok(out)
            }
            GithubReadKind::Comments => {
                let n = read.number.ok_or_else(|| missing("number"))?;
                let issue: RawIssue = self
                    .get_json(
                        &format!("{}/issues/{n}", r.path()),
                        &format!("{r}'s issue #{n}"),
                    )
                    .await?;
                let mut comments: Vec<RawComment> = Vec::new();
                if issue.comments > 0 {
                    let last = issue.comments.div_ceil(COMMENTS_READ);
                    let first = last.saturating_sub(1).max(1);
                    for page in first..=last {
                        let page_comments: Vec<RawComment> = self
                            .get_json(
                                &format!(
                                    "{}/issues/{n}/comments?per_page={COMMENTS_READ}&page={page}",
                                    r.path()
                                ),
                                &format!("the comments on {r}#{n}"),
                            )
                            .await?;
                        comments.extend(page_comments);
                    }
                }
                let keep = usize::try_from(COMMENTS_READ).unwrap_or(usize::MAX);
                let from = comments.len().saturating_sub(keep);
                if comments.is_empty() {
                    return Ok(format!("{r}#{n} has no comments."));
                }
                let mut out = format!("The newest comments on {r}#{n}:\n");
                for c in &comments[from..] {
                    let _ = write!(
                        out,
                        "\n— {} at {}:\n{}\n",
                        author(c.user.as_ref()),
                        c.created_at.as_deref().unwrap_or("?"),
                        c.body.as_deref().unwrap_or("")
                    );
                }
                Ok(out)
            }
            GithubReadKind::File => {
                let path = read.path.as_deref().ok_or_else(|| missing("path"))?;
                let path = path.trim_start_matches('/');
                let mut uri = format!("{}/contents/{}", r.path(), encode_path(path));
                if let Some(git_ref) = &read.git_ref {
                    let _ = write!(uri, "?ref={}", encode_query(git_ref));
                }
                let file: RawFile = self.get_json(&uri, &format!("{path} in {r}")).await?;
                if file.kind != "file" {
                    return Ok(format!("{path} in {r} is a {}, not a file.", file.kind));
                }
                if file.size > FILE_MAX_BYTES {
                    return Ok(format!(
                        "{path} in {r} is {} bytes, larger than the 200 KB github_read shows.",
                        file.size
                    ));
                }
                let content = file.content.unwrap_or_default();
                if file.encoding.as_deref() != Some("base64") {
                    return Ok(content);
                }
                let compact: String = content.chars().filter(|c| !c.is_whitespace()).collect();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(compact)
                    .map_err(|e| GithubError::Decode(e.to_string()))?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
        }
    }

    /// Writes a comment, adds or removes labels, or opens an issue; the URL of what was written
    /// when GitHub gives one.
    pub async fn write(
        &self,
        r: &RepoRef,
        write: &GithubWrite,
    ) -> Result<Option<String>, GithubError> {
        match write.action {
            GithubWriteAction::Comment => {
                let n = write.number.ok_or_else(|| missing("number"))?;
                let body = write.body.as_deref().ok_or_else(|| missing("body"))?;
                let request = serde_json::json!({ "body": body });
                let response = self
                    .send(
                        Method::Post,
                        &format!("{}/issues/{n}/comments", r.path()),
                        Some(&request),
                    )
                    .await?;
                if !response.ok() {
                    return Err(response.error(&format!("{r}#{n}"), self.rate()));
                }
                Ok(response.json::<RawCreated>()?.html_url)
            }
            GithubWriteAction::AddLabels => {
                let n = write.number.ok_or_else(|| missing("number"))?;
                let request = serde_json::json!({ "labels": write.labels });
                let response = self
                    .send(
                        Method::Post,
                        &format!("{}/issues/{n}/labels", r.path()),
                        Some(&request),
                    )
                    .await?;
                if !response.ok() {
                    return Err(response.error(&format!("{r}#{n}"), self.rate()));
                }
                Ok(None)
            }
            GithubWriteAction::RemoveLabels => {
                let n = write.number.ok_or_else(|| missing("number"))?;
                for label in &write.labels {
                    let response = self
                        .send(
                            Method::Delete,
                            &format!("{}/issues/{n}/labels/{}", r.path(), encode_query(label)),
                            None,
                        )
                        .await?;
                    if !response.ok() && response.status != 404 {
                        return Err(response.error(&format!("{r}#{n}"), self.rate()));
                    }
                }
                Ok(None)
            }
            GithubWriteAction::OpenIssue => {
                let title = write.title.as_deref().ok_or_else(|| missing("title"))?;
                let request = serde_json::json!({
                    "title": title,
                    "body": write.body.as_deref().unwrap_or(""),
                    "labels": write.labels,
                });
                let response = self
                    .send(
                        Method::Post,
                        &format!("{}/issues", r.path()),
                        Some(&request),
                    )
                    .await?;
                if !response.ok() {
                    return Err(response.error(&format!("{r}'s issues"), self.rate()));
                }
                Ok(response.json::<RawCreated>()?.html_url)
            }
        }
    }
}
