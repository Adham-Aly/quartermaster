//! The HTTP every login shares: one client with timeouts, form and JSON requests, and errors that
//! never carry a response body.
//!
//! A token endpoint's body holds tokens on success and, on failure, sometimes echoes what was sent.
//! So nothing here ever puts a body into an error or a log line: errors name the operation, the
//! HTTP status and the provider's error code (cut short, code-like characters only).

use crate::AuthError;
use std::time::Duration;

/// How long connecting may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one request may take, start to finish.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The client every login uses: `quartermaster/<version>`, with timeouts.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("quartermaster/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// A response: its status and its JSON object (empty when the body isn't a JSON object).
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub body: serde_json::Map<String, serde_json::Value>,
    /// The body parsed as JSON at all (an object or not).
    pub json: bool,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A string field, if present and non-empty.
    pub fn str(&self, field: &str) -> Option<&str> {
        self.body
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
    }

    /// The provider's OAuth error code (`{"error":"…"}` or `{"error":{"code":"…"}}`), cut short.
    pub fn error_code(&self) -> Option<String> {
        let error = self.body.get("error")?;
        let code = error
            .as_str()
            .or_else(|| error.get("code").and_then(serde_json::Value::as_str))
            .or_else(|| error.get("type").and_then(serde_json::Value::as_str))?;
        Some(short_code(code))
    }

    /// The failure for a refused request: [`AuthError::Rejected`] when the credentials themselves
    /// were refused (401, 403, or an OAuth code saying so), [`AuthError::Login`] otherwise.
    pub fn failure(&self, provider: &str, operation: &str) -> AuthError {
        let code = self.error_code();
        let shown = code
            .as_deref()
            .map_or_else(String::new, |c| format!(": {c}"));
        let message = format!(
            "{provider} refused the {operation} (HTTP {}){shown}",
            self.status
        );
        if is_rejection(self.status, code.as_deref()) {
            AuthError::Rejected(message)
        } else {
            AuthError::Login(message)
        }
    }
}

/// Whether a refusal means the credentials are no good (revoked, expired for good, invalid).
pub(crate) fn is_rejection(status: u16, code: Option<&str>) -> bool {
    matches!(status, 401 | 403)
        || matches!(
            code,
            Some(
                "invalid_grant"
                    | "invalid_token"
                    | "invalid_client"
                    | "unauthorized_client"
                    | "access_denied"
                    | "token_revoked"
                    | "token_expired"
                    | "refresh_token_expired"
                    | "refresh_token_reused"
                    | "refresh_token_invalidated"
            )
        )
}

/// Posts a form (`application/x-www-form-urlencoded`).
pub(crate) async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    provider: &str,
    operation: &str,
) -> Result<Reply, AuthError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let request = http
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body);
    send(request, provider, operation).await
}

/// Posts a JSON body.
pub(crate) async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    provider: &str,
    operation: &str,
) -> Result<Reply, AuthError> {
    let request = http
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(body.to_string());
    send(request, provider, operation).await
}

/// Sends a request built by the caller (headers and all).
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    provider: &str,
    operation: &str,
) -> Result<Reply, AuthError> {
    let response = request.send().await.map_err(|e| {
        AuthError::Login(format!(
            "the {provider} {operation} request failed: {}",
            network_reason(&e)
        ))
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    let json = parsed.is_some();
    let body = match parsed {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    Ok(Reply { status, body, json })
}

/// A provider's error code, limited to code-like characters and 64 of them.
pub(crate) fn short_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect()
}

/// Why a request never got an answer, in words (never the URL: a query may carry a code).
pub(crate) fn network_reason(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "couldn't connect"
    } else {
        "network error"
    }
}

/// A positive, finite number field (`expires_in`, `interval`).
pub(crate) fn positive_number(reply: &Reply, field: &str) -> Option<f64> {
    reply
        .body
        .get(field)
        .and_then(serde_json::Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
}

/// A URL the app may open in your browser: `https`, or `http` on this Mac only (local fakes in
/// tests). Anything else is refused, so a hostile response can't make `open` launch something.
pub(crate) fn trusted_browser_url(raw: &str, provider: &str) -> Result<String, AuthError> {
    let url = url::Url::parse(raw.trim()).map_err(|_| {
        AuthError::Login(format!(
            "{provider} sent a verification address that isn't a URL"
        ))
    })?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match url.scheme() {
        "https" => Ok(url.into()),
        "http" if local => Ok(url.into()),
        _ => Err(AuthError::Login(format!(
            "{provider} sent a verification address that isn't https; it was not opened"
        ))),
    }
}
