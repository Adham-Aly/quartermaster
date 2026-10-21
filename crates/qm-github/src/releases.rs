//! A public repository's newest release, read without a login.
//!
//! The app's own update check uses this: it must work on a Mac where `gh` is missing or logged
//! out, so it asks GitHub's API anonymously (60 calls an hour per address, far more than a check
//! every few hours needs) and never touches the token source.

use crate::client::CALL_TIMEOUT;
use crate::error::GithubError;
use crate::repo::RepoRef;
use octocrab::Octocrab;
use octocrab::service::middleware::retry::RetryConfig;
use serde::Deserialize;
use std::time::Duration;

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct Release {
    /// Its tag: "v0.2.0".
    #[serde(rename = "tag_name")]
    pub tag: String,
    /// Its page on github.com.
    #[serde(rename = "html_url")]
    pub page: String,
}

impl Release {
    pub fn new(tag: impl Into<String>, page: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            page: page.into(),
        }
    }
}

/// The newest release of `repo` that isn't a draft or a pre-release, asked anonymously.
pub async fn latest_release(repo: &RepoRef) -> Result<Release, GithubError> {
    let octo = Octocrab::builder()
        .add_retry_config(RetryConfig::None)
        .set_connect_timeout(Some(CONNECT_TIMEOUT))
        .set_read_timeout(Some(CALL_TIMEOUT))
        .build()
        .map_err(|e| GithubError::Network(e.to_string()))?;
    let what = format!("{repo}'s latest release");
    let path = format!("/repos/{}/{}/releases/latest", repo.owner, repo.name);
    let call = async {
        let response = octo
            ._get(path)
            .await
            .map_err(|e| GithubError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let body = octo
            .body_to_string(response)
            .await
            .map_err(|e| GithubError::Network(e.to_string()))?;
        Ok::<_, GithubError>((status, body))
    };
    let (status, body) = tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| GithubError::Network("GitHub didn't answer within 30 seconds".to_owned()))??;
    match status {
        200..=299 => serde_json::from_str(&body).map_err(|e| GithubError::Decode(e.to_string())),
        404 => Err(GithubError::NotFound(what)),
        403 | 429 => Err(GithubError::RateLimited { reset_at: None }),
        status => Err(GithubError::Http {
            status,
            message: serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
                .unwrap_or_else(|| format!("status {status}")),
        }),
    }
}
