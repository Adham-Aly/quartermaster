//! The client: one `Octocrab` per token value, every call with a time limit, the rate limit
//! tracked from each response.
//!
//! Calls go through octocrab's plain request methods with the app's own types, so the status code
//! of every answer is seen (the merge's 409 and 405 matter). Octocrab's own retries
//! are switched off: a merge is never sent twice without a read-back first. On a 401 the client
//! asks the token source again and retries that call once.

use crate::error::{GithubError, RateState};
use crate::token::{GithubToken, TokenSource};
use jiff::Timestamp;
use octocrab::Octocrab;
use octocrab::service::middleware::retry::RetryConfig;
use serde::de::DeserializeOwned;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Every call's time limit.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// GitHub's API through octocrab, with the token from a [`TokenSource`]. Cheap to clone.
#[derive(Clone)]
pub struct GithubClient {
    inner: Arc<Inner>,
}

struct Inner {
    tokens: Arc<dyn TokenSource>,
    /// The client for the token it was built with (the token itself is never printed).
    octo: Mutex<Option<(GithubToken, Octocrab)>>,
    rate: Mutex<RateState>,
}

impl fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubClient")
            .field("tokens", &self.inner.tokens)
            .finish_non_exhaustive()
    }
}

/// An HTTP method the client uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// A response: its status and body.
#[derive(Clone, Debug)]
pub(crate) struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub(crate) fn json<T: DeserializeOwned>(&self) -> Result<T, GithubError> {
        serde_json::from_str(&self.body).map_err(|e| GithubError::Decode(e.to_string()))
    }

    /// GitHub's own message from an error body, or the status.
    pub(crate) fn message(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("status {}", self.status))
    }

    /// This response as an error; `what` names the thing asked for ("api-server's pull request
    /// #212").
    pub(crate) fn error(&self, what: &str, rate: RateState) -> GithubError {
        match self.status {
            401 => GithubError::Auth,
            404 => GithubError::NotFound(what.to_owned()),
            403 | 429 if rate.remaining == Some(0) || self.status == 429 => {
                GithubError::RateLimited {
                    reset_at: rate.reset_at,
                }
            }
            status => GithubError::Http {
                status,
                message: self.message(),
            },
        }
    }
}

impl GithubClient {
    /// The client for api.github.com.
    pub fn new(tokens: Arc<dyn TokenSource>) -> Self {
        Self {
            inner: Arc::new(Inner {
                tokens,
                octo: Mutex::new(None),
                rate: Mutex::new(RateState::default()),
            }),
        }
    }

    /// The rate limit as the last response reported it.
    pub fn rate(&self) -> RateState {
        *self
            .inner
            .rate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn octocrab(&self, token: &GithubToken) -> Result<Octocrab, GithubError> {
        let mut guard = self
            .inner
            .octo
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((built_for, octo)) = guard.as_ref()
            && built_for == token
        {
            return Ok(octo.clone());
        }
        let octo = Octocrab::builder()
            .add_retry_config(RetryConfig::None)
            .set_connect_timeout(Some(CONNECT_TIMEOUT))
            .set_read_timeout(Some(CALL_TIMEOUT))
            .personal_token(token.expose().to_owned())
            .build()
            .map_err(|e| GithubError::Network(e.to_string()))?;
        *guard = Some((token.clone(), octo.clone()));
        Ok(octo)
    }

    fn note_rate(&self, remaining: Option<&str>, reset: Option<&str>) {
        let remaining = remaining.and_then(|v| v.trim().parse::<u32>().ok());
        let reset_at = reset
            .and_then(|v| v.trim().parse::<i64>().ok())
            .and_then(|s| Timestamp::from_second(s).ok());
        if remaining.is_none() && reset_at.is_none() {
            return;
        }
        let mut rate = self
            .inner
            .rate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if remaining.is_some() {
            rate.remaining = remaining;
        }
        if reset_at.is_some() {
            rate.reset_at = reset_at;
        }
    }

    /// One call: `path` is the API path with its query (`/repos/o/r/pulls/1`). A 401 asks the
    /// token source again and retries once. Any status comes back as a [`Response`]; only a
    /// call that got no answer is an error.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, GithubError> {
        for attempt in 0..2 {
            let token = self.inner.tokens.token().await?;
            let octo = self.octocrab(&token)?;
            let uri = path.to_owned();
            let call = async {
                match method {
                    Method::Get => octo._get(uri).await,
                    Method::Post => octo._post(uri, body).await,
                    Method::Put => octo._put(uri, body).await,
                    Method::Patch => octo._patch(uri, body).await,
                    Method::Delete => octo._delete(uri, body).await,
                }
            };
            let response = match tokio::time::timeout(CALL_TIMEOUT, call).await {
                Err(_) => {
                    return Err(GithubError::Network(
                        "GitHub didn't answer within 30 seconds".to_owned(),
                    ));
                }
                Ok(Err(e)) => return Err(GithubError::Network(e.to_string())),
                Ok(Ok(response)) => response,
            };
            let status = response.status().as_u16();
            {
                let headers = response.headers();
                let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
                self.note_rate(header("x-ratelimit-remaining"), header("x-ratelimit-reset"));
            }
            let body = match tokio::time::timeout(CALL_TIMEOUT, octo.body_to_string(response)).await
            {
                Err(_) => {
                    return Err(GithubError::Network(
                        "GitHub's answer didn't arrive within 30 seconds".to_owned(),
                    ));
                }
                Ok(Err(e)) => return Err(GithubError::Network(e.to_string())),
                Ok(Ok(body)) => body,
            };
            if status == 401 && attempt == 0 {
                self.inner.tokens.invalidate();
                continue;
            }
            if status == 401 {
                return Err(GithubError::Auth);
            }
            return Ok(Response { status, body });
        }
        Err(GithubError::Auth)
    }

    /// A GET asking for the media type `accept`, whose answer is refused unread past `cap` bytes
    /// (a `Decode` error). A 401 asks the token source again and retries once, as [`Self::send`].
    pub(crate) async fn get_capped(
        &self,
        path: &str,
        accept: &'static str,
        cap: usize,
    ) -> Result<Response, GithubError> {
        use http_body_util::BodyExt;
        let too_large = || {
            GithubError::Decode(format!(
                "GitHub's answer was larger than {} KiB, so it wasn't read",
                cap / 1024
            ))
        };
        for attempt in 0..2 {
            let token = self.inner.tokens.token().await?;
            let octo = self.octocrab(&token)?;
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::ACCEPT, http::HeaderValue::from_static(accept));
            let call = octo._get_with_headers(path.to_owned(), Some(headers));
            let response = match tokio::time::timeout(CALL_TIMEOUT, call).await {
                Err(_) => {
                    return Err(GithubError::Network(
                        "GitHub didn't answer within 30 seconds".to_owned(),
                    ));
                }
                Ok(Err(e)) => return Err(GithubError::Network(e.to_string())),
                Ok(Ok(response)) => response,
            };
            let status = response.status().as_u16();
            let declared = {
                let headers = response.headers();
                let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
                self.note_rate(header("x-ratelimit-remaining"), header("x-ratelimit-reset"));
                header("content-length").and_then(|v| v.trim().parse::<u64>().ok())
            };
            if declared.is_some_and(|n| n > u64::try_from(cap).unwrap_or(u64::MAX)) {
                return Err(too_large());
            }
            let limited = http_body_util::Limited::new(response.into_body(), cap);
            let bytes = match tokio::time::timeout(CALL_TIMEOUT, limited.collect()).await {
                Err(_) => {
                    return Err(GithubError::Network(
                        "GitHub's answer didn't arrive within 30 seconds".to_owned(),
                    ));
                }
                Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => {
                    return Err(too_large());
                }
                Ok(Err(e)) => return Err(GithubError::Network(e.to_string())),
                Ok(Ok(collected)) => collected.to_bytes(),
            };
            let body = String::from_utf8(bytes.to_vec())
                .map_err(|_| GithubError::Decode("GitHub's answer wasn't UTF-8".to_owned()))?;
            if status == 401 && attempt == 0 {
                self.inner.tokens.invalidate();
                continue;
            }
            if status == 401 {
                return Err(GithubError::Auth);
            }
            return Ok(Response { status, body });
        }
        Err(GithubError::Auth)
    }

    /// A GET whose success is decoded as `T`; anything else is an error naming `what`.
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<T, GithubError> {
        let response = self.send(Method::Get, path, None).await?;
        if !response.ok() {
            return Err(response.error(what, self.rate()));
        }
        response.json()
    }
}

fn percent_encode(s: &str, keep: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || keep.contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xf)]));
        }
    }
    out
}

/// Percent-encodes a path segment that may hold slashes (a branch name), keeping the slashes.
pub(crate) fn encode_path(s: &str) -> String {
    percent_encode(s, b"-._~/")
}

/// Percent-encodes a query value.
pub(crate) fn encode_query(s: &str) -> String {
    percent_encode(s, b"-._~")
}
