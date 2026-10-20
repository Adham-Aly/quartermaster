//! The engine's HTTP client: one per engine, shared by every session.
//!
//! Sends an adapter's [`HttpRequest`] with the credential where the adapter says it goes
//! ([`CredentialHeader`]: a bearer token by default, `x-api-key` for Anthropic keys,
//! `x-goog-api-key` for Gemini keys), marked sensitive so no `Debug` output shows it, and hands back
//! the response for streaming. No timeouts of
//! reqwest's own.
//! The stream's idle limit is the loop's, on the injected `Clock`.

use crate::adapters::{CredentialHeader, HttpRequest};
use qm_auth::Secret;
use qm_engine::{ProviderError, ProviderErrorKind};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

/// The shared client.
#[derive(Clone, Debug)]
pub struct HttpClient {
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new(user_agent: &str) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .build()
            .unwrap_or_default();
        Self { client }
    }

    /// Sends `request` with `credential`, when given, in the header the adapter named. A network
    /// failure is a `Network` provider error naming the host, never the request; a credential
    /// that isn't a valid header value is an `Auth` error that never shows it.
    pub async fn send(
        &self,
        request: &HttpRequest,
        credential: Option<(&Secret, CredentialHeader)>,
    ) -> Result<reqwest::Response, ProviderError> {
        let body = serde_json::to_vec(&request.body).map_err(|e| ProviderError {
            kind: ProviderErrorKind::InvalidRequest,
            status: None,
            message: format!("couldn't encode the request: {e}"),
        })?;
        let mut builder = self.client.post(&request.url).body(body);
        for (name, value) in &request.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            } else {
                tracing::debug!(header = %name, "dropping a header that isn't valid HTTP");
            }
        }
        if let Some((secret, placement)) = credential {
            let invalid = || ProviderError {
                kind: ProviderErrorKind::Auth,
                status: None,
                message: "the stored credential isn't a valid header value".into(),
            };
            let (name, text) = match placement {
                CredentialHeader::Bearer => (AUTHORIZATION, format!("Bearer {}", secret.expose())),
                CredentialHeader::Named(name) => (
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?,
                    secret.expose().to_owned(),
                ),
            };
            let mut value = HeaderValue::from_str(&text).map_err(|_| invalid())?;
            value.set_sensitive(true);
            builder = builder.header(name, value);
        }
        builder
            .send()
            .await
            .map_err(|e| network_error(&request.url, &e))
    }
}

/// A network-level failure as a provider error: the host and the kind of failure only.
pub fn network_error(url: &str, e: &reqwest::Error) -> ProviderError {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "the provider".to_owned());
    let reason = if e.is_timeout() {
        "the request timed out"
    } else if e.is_connect() {
        "couldn't connect"
    } else if e.is_body() || e.is_decode() {
        "the connection broke mid-response"
    } else {
        "a network error"
    };
    ProviderError {
        kind: ProviderErrorKind::Network,
        status: None,
        message: format!("{host}: {reason}"),
    }
}
