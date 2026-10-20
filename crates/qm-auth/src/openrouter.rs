//! OpenRouter login in the browser.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/openrouter.ts` (MIT, Copyright Earendil Works).
//! OpenRouter's PKCE flow trades the authorization code for a **permanent API key** that you
//! control on openrouter.ai, not for expiring tokens, so the result is a
//! [`Credential::ApiKey`] and there is nothing to refresh. OpenRouter sends no `state`: the
//! callback listens on a free port at a random path, so a stray request can't finish the login.

use crate::callback::{CallbackServer, CallbackVerdict};
use crate::cancel::Cancel;
use crate::http;
use crate::pkce::Pkce;
use crate::{AuthError, Credential, Secret};
use std::fmt;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

/// How long the login waits for the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const NAME: &str = "OpenRouter";

/// Where the OpenRouter login talks. [`Default`] is OpenRouter's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRouterEndpoints {
    pub authorize_url: String,
    /// Trades the code for a key.
    pub keys_url: String,
    /// The local callback port; 0 picks a free one.
    pub callback_port: u16,
}

impl Default for OpenRouterEndpoints {
    fn default() -> Self {
        Self {
            authorize_url: "https://openrouter.ai/auth".into(),
            keys_url: "https://openrouter.ai/api/v1/auth/keys".into(),
            callback_port: 0,
        }
    }
}

/// An OpenRouter login waiting for the browser.
pub struct PendingOpenRouter {
    http: reqwest::Client,
    keys_url: String,
    pkce: Pkce,
    authorize_url: String,
    redirect_uri: String,
    server: CallbackServer,
}

impl fmt::Debug for PendingOpenRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The redirect URI's random path is what keeps stray requests out: not shown.
        f.debug_struct("PendingOpenRouter")
            .field("callback_port", &self.server.port())
            .finish_non_exhaustive()
    }
}

/// Binds the callback and builds the authorize URL.
pub async fn start(
    http: &reqwest::Client,
    endpoints: &OpenRouterEndpoints,
) -> Result<PendingOpenRouter, AuthError> {
    let path = format!("/oauth/callback/{}", Uuid::new_v4().simple());
    let server = CallbackServer::bind_with_path(endpoints.callback_port, &path).await?;
    let redirect_uri = server.redirect_uri();
    let pkce = Pkce::generate();
    let mut url = Url::parse(&endpoints.authorize_url)
        .map_err(|e| AuthError::Login(format!("the OpenRouter authorize URL is invalid: {e}")))?;
    url.query_pairs_mut()
        .append_pair("callback_url", &redirect_uri)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(PendingOpenRouter {
        http: http.clone(),
        keys_url: endpoints.keys_url.clone(),
        pkce,
        authorize_url: url.into(),
        redirect_uri,
        server,
    })
}

impl PendingOpenRouter {
    /// The URL to open in the browser.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// Waits for the browser's callback (at most `timeout`, or until cancelled), then trades the
    /// code for a key.
    pub async fn finish(self, timeout: Duration, cancel: &Cancel) -> Result<Credential, AuthError> {
        let Self {
            http,
            keys_url,
            pkce,
            server,
            ..
        } = self;
        let wait = server.wait(|url| match code_from(url) {
            Ok(code) => CallbackVerdict::Accept(code),
            Err(CodeProblem::BrowserError(e)) => CallbackVerdict::Fail(e),
            Err(CodeProblem::Missing) => {
                CallbackVerdict::Reject("Missing authorization code.".into())
            }
        });
        let code = tokio::select! {
            () = cancel.cancelled() => return Err(AuthError::Cancelled),
            result = tokio::time::timeout(timeout, wait) => result.map_err(|_| {
                AuthError::Expired("the OpenRouter login timed out waiting for the browser".into())
            })??,
        };
        tokio::select! {
            () = cancel.cancelled() => Err(AuthError::Cancelled),
            result = exchange(&http, &keys_url, &code, &pkce.verifier) => result,
        }
    }

    /// Finishes with the address the browser ended on, or the bare code, pasted by you.
    pub async fn finish_with_pasted(self, pasted: &str) -> Result<Credential, AuthError> {
        let pasted = pasted.trim();
        let code = match Url::parse(pasted) {
            Ok(url) => {
                if url.path() != self.server.path() {
                    return Err(AuthError::Login(format!(
                        "the pasted address must start with {}",
                        self.redirect_uri
                    )));
                }
                code_from(&url).map_err(|e| match e {
                    CodeProblem::BrowserError(e) => AuthError::Login(e),
                    CodeProblem::Missing => {
                        AuthError::Login("the pasted address has no authorization code".into())
                    }
                })?
            }
            Err(_) if !pasted.is_empty() && !pasted.contains(char::is_whitespace) => {
                pasted.to_owned()
            }
            Err(_) => {
                return Err(AuthError::Login(
                    "paste the full address from the browser, or the code".into(),
                ));
            }
        };
        exchange(&self.http, &self.keys_url, &code, &self.pkce.verifier).await
    }
}

enum CodeProblem {
    BrowserError(String),
    Missing,
}

fn code_from(url: &Url) -> Result<String, CodeProblem> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(error) = param("error") {
        return Err(CodeProblem::BrowserError(format!(
            "OpenRouter authorization failed: {}",
            http::short_code(&error)
        )));
    }
    param("code")
        .filter(|c| !c.trim().is_empty())
        .ok_or(CodeProblem::Missing)
}

async fn exchange(
    http: &reqwest::Client,
    keys_url: &str,
    code: &str,
    verifier: &str,
) -> Result<Credential, AuthError> {
    let reply = http::post_json(
        http,
        keys_url,
        &serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }),
        NAME,
        "key exchange",
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure(NAME, "key exchange"));
    }
    let key = reply
        .str("key")
        .ok_or_else(|| AuthError::Login("OpenRouter's answer carries no key".to_owned()))?;
    Ok(Credential::ApiKey {
        key: Secret::new(key),
    })
}
