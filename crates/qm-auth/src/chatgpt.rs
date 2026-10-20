//! Sign in with ChatGPT, and the Codex login as its fallback.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/openai-chatgpt.ts` and `openai-codex.ts` (MIT,
//! Copyright Earendil Works), and OpenAI Codex `codex-rs/login/src/{pkce,server,token_data}.rs`
//! (Apache-2.0, Copyright OpenAI; adapted, not copied: the flow's parameters, the callback checks
//! and the JWT claim path follow them).
//!
//! Two flows share PKCE, the callback server and the token parsing:
//!
//! - [`ChatGptFlow::SignInWithChatGpt`] (primary): registers a new public client on every login
//!   (`client_id=dynamic_agent_client`); the callback carries the **issued** client ID, which the
//!   code exchange and every refresh must use. Tokens go straight to `api.openai.com/v1`.
//! - [`ChatGptFlow::Codex`] (fallback, if plan usage isn't offered to a private app): the Codex
//!   CLI's public client; the account ID comes from the access token's JWT claims, and requests go
//!   to the ChatGPT backend (`chatgpt.com/backend-api/codex`).
//!
//! The provider engine tells them apart by the stored credential: an `account_id` means Codex.
//!
//! **Never logged**: no token, code, verifier, `state`, `nonce` or callback query string appears in
//! a log line, an error or an event. Errors carry the HTTP status and the provider's error code
//! only.

use crate::callback::{CALLBACK_PATH, CallbackServer, CallbackVerdict};
use crate::cancel::Cancel;
use crate::device_code::{self, DeviceCode, Poll};
use crate::pkce::{self, Pkce};
use crate::{AuthError, Credential, Secret};
use base64::Engine as _;
use jiff::{SignedDuration, Timestamp};
use qm_types::data_dir::DataDir;
use std::fmt;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

/// Sign in with ChatGPT's fixed registration client: every login registers a new client with it.
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// The Codex CLI's public client (the fallback flow).
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// What Sign in with ChatGPT shows as the app's name.
pub const AGENT_NAME_HINT: &str = "Quartermaster";
/// The resource the Sign in with ChatGPT tokens are for.
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// The scope that lets the token reach the Responses API directly.
pub const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// Sign in with ChatGPT's scopes.
pub const SIGN_IN_SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The Codex login's scopes.
pub const CODEX_SCOPE: &str = "openid profile email offline_access";
/// The callback port both flows use (shared with the Codex CLI).
pub const CALLBACK_PORT: u16 = 1455;
/// The JWT claim object that holds `chatgpt_account_id`.
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// Tokens are treated as expired this long before they really are, so no request starts with a
/// token about to lapse; the engine refreshes inside the same window.
pub const EXPIRY_MARGIN: Duration = Duration::from_mins(3);

/// Which login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatGptFlow {
    SignInWithChatGpt,
    Codex,
}

impl ChatGptFlow {
    /// The flow a stored credential came from: an account ID means the Codex login.
    pub fn of(credential: &Credential) -> Option<Self> {
        match credential {
            Credential::OAuth {
                account_id: Some(_),
                ..
            } => Some(Self::Codex),
            Credential::OAuth { .. } => Some(Self::SignInWithChatGpt),
            Credential::ApiKey { .. } => None,
        }
    }
}

/// Where both flows authorize and exchange tokens. [`Default`] is OpenAI's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptEndpoints {
    /// Sign in with ChatGPT's authorize URL.
    pub authorize_url: String,
    /// Sign in with ChatGPT's token URL (code exchange and refresh).
    pub token_url: String,
    /// The Codex login's authorize URL.
    pub codex_authorize_url: String,
    /// The Codex login's token URL.
    pub codex_token_url: String,
    /// The Codex device login: where the user code is requested.
    pub codex_device_usercode_url: String,
    /// The Codex device login: where polling asks for the authorization code.
    pub codex_device_token_url: String,
    /// The Codex device login: the page you enter the code on.
    pub codex_device_verification_url: String,
    /// The Codex device login: the redirect URI its code exchange names.
    pub codex_device_redirect_uri: String,
    /// The local callback port.
    pub callback_port: u16,
}

impl Default for ChatGptEndpoints {
    fn default() -> Self {
        Self::at_origin("https://auth.openai.com", CALLBACK_PORT)
    }
}

impl ChatGptEndpoints {
    /// Every URL on `origin` (no trailing slash), with the real paths.
    fn at_origin(origin: &str, callback_port: u16) -> Self {
        let origin = origin.trim_end_matches('/');
        Self {
            authorize_url: format!("{origin}/api/accounts/authorize"),
            token_url: format!("{origin}/api/accounts/oauth/token"),
            codex_authorize_url: format!("{origin}/oauth/authorize"),
            codex_token_url: format!("{origin}/oauth/token"),
            codex_device_usercode_url: format!("{origin}/api/accounts/deviceauth/usercode"),
            codex_device_token_url: format!("{origin}/api/accounts/deviceauth/token"),
            codex_device_verification_url: format!("{origin}/codex/device"),
            codex_device_redirect_uri: format!("{origin}/deviceauth/callback"),
            callback_port,
        }
    }

    fn authorize_url(&self, flow: ChatGptFlow) -> &str {
        match flow {
            ChatGptFlow::SignInWithChatGpt => &self.authorize_url,
            ChatGptFlow::Codex => &self.codex_authorize_url,
        }
    }

    fn token_url(&self, flow: ChatGptFlow) -> &str {
        match flow {
            ChatGptFlow::SignInWithChatGpt => &self.token_url,
            ChatGptFlow::Codex => &self.codex_token_url,
        }
    }

    /// The redirect URI the flow registers, on the port the callback server really bound (with
    /// `callback_port` 0 that is a free port, not 0). Sign in with ChatGPT insists on
    /// `127.0.0.1`; the Codex client was registered with `localhost`.
    fn redirect_uri(flow: ChatGptFlow, bound_port: u16) -> String {
        let host = match flow {
            ChatGptFlow::SignInWithChatGpt => "127.0.0.1",
            ChatGptFlow::Codex => "localhost",
        };
        format!("http://{host}:{bound_port}{CALLBACK_PATH}")
    }
}

/// Starts logins of one flow.
#[derive(Debug)]
pub struct ChatGptLogin {
    flow: ChatGptFlow,
    endpoints: ChatGptEndpoints,
    install_id: Uuid,
    http: reqwest::Client,
}

impl ChatGptLogin {
    /// `install_id` is this installation's stable host ID ([`install_id`]); Sign in with ChatGPT
    /// sends it as `ext_agent_host_id`.
    pub fn new(flow: ChatGptFlow, endpoints: ChatGptEndpoints, install_id: Uuid) -> Self {
        Self {
            flow,
            endpoints,
            install_id,
            http: crate::http::client(),
        }
    }

    /// Binds the callback port and builds the authorize URL. The app opens the URL with `open`
    /// and shows it too, then calls [`PendingLogin::finish`] (or, when the browser can't reach
    /// the port, [`PendingLogin::finish_with_pasted_url`]).
    pub async fn start(&self) -> Result<PendingLogin, AuthError> {
        let server = CallbackServer::bind(self.endpoints.callback_port).await?;
        let pkce = Pkce::generate();
        let redirect_uri = ChatGptEndpoints::redirect_uri(self.flow, server.port());
        let mut url = Url::parse(self.endpoints.authorize_url(self.flow))
            .map_err(|e| AuthError::Login(format!("the authorize URL is invalid: {e}")))?;
        let state;
        match self.flow {
            ChatGptFlow::SignInWithChatGpt => {
                state = pkce::random_base64url(32);
                let nonce = pkce::random_base64url(32);
                url.query_pairs_mut()
                    .append_pair("client_id", DYNAMIC_CLIENT_ID)
                    .append_pair("agent_name_hint", AGENT_NAME_HINT)
                    .append_pair(
                        "ext_agent_host_id",
                        &format!("urn:uuid:{}", self.install_id.hyphenated()),
                    )
                    .append_pair("response_type", "code")
                    .append_pair("redirect_uri", &redirect_uri)
                    .append_pair("resource", RESOURCE)
                    .append_pair("scope", SIGN_IN_SCOPE)
                    .append_pair("state", &state)
                    .append_pair("code_challenge", &pkce.challenge)
                    .append_pair("code_challenge_method", "S256")
                    .append_pair("nonce", &nonce);
            }
            ChatGptFlow::Codex => {
                state = pkce::random_hex(16);
                url.query_pairs_mut()
                    .append_pair("response_type", "code")
                    .append_pair("client_id", CODEX_CLIENT_ID)
                    .append_pair("redirect_uri", &redirect_uri)
                    .append_pair("scope", CODEX_SCOPE)
                    .append_pair("code_challenge", &pkce.challenge)
                    .append_pair("code_challenge_method", "S256")
                    .append_pair("state", &state)
                    .append_pair("id_token_add_organizations", "true")
                    .append_pair("codex_cli_simplified_flow", "true")
                    .append_pair("originator", "quartermaster");
            }
        }
        Ok(PendingLogin {
            flow: self.flow,
            endpoints: self.endpoints.clone(),
            http: self.http.clone(),
            pkce,
            state,
            redirect_uri,
            authorize_url: url.into(),
            server,
        })
    }
}

/// A login waiting for its callback.
pub struct PendingLogin {
    flow: ChatGptFlow,
    endpoints: ChatGptEndpoints,
    http: reqwest::Client,
    pkce: Pkce,
    state: String,
    redirect_uri: String,
    authorize_url: String,
    server: CallbackServer,
}

impl fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The authorize URL carries `state`: not shown.
        f.debug_struct("PendingLogin")
            .field("flow", &self.flow)
            .field("callback_port", &self.server.port())
            .finish_non_exhaustive()
    }
}

/// What a good callback hands the code exchange.
struct Authorization {
    code: String,
    /// The issued client ID (Sign in with ChatGPT only).
    client_id: Option<String>,
}

impl PendingLogin {
    /// The URL to open in the browser.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// The port the callback server listens on.
    pub fn callback_port(&self) -> u16 {
        self.server.port()
    }

    /// Waits for the browser's callback (at most `timeout`), then exchanges the code.
    pub async fn finish(self, timeout: Duration) -> Result<Credential, AuthError> {
        self.finish_or_cancel(timeout, &Cancel::new()).await
    }

    /// As [`PendingLogin::finish`], ending with [`AuthError::Cancelled`] as soon as `cancel` is
    /// pressed.
    pub async fn finish_or_cancel(
        self,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Credential, AuthError> {
        tokio::select! {
            () = cancel.cancelled() => Err(AuthError::Cancelled),
            result = self.finish_inner(timeout) => result,
        }
    }

    async fn finish_inner(self, timeout: Duration) -> Result<Credential, AuthError> {
        let Self {
            flow,
            endpoints,
            http,
            pkce,
            state,
            redirect_uri,
            server,
            ..
        } = self;
        let wait = server.wait(|url| match authorization_from_callback(flow, url, &state) {
            Ok(auth) => CallbackVerdict::Accept(auth),
            Err(CallbackProblem::BrowserError(e)) => CallbackVerdict::Fail(e),
            Err(CallbackProblem::Invalid(e)) => CallbackVerdict::Reject(e),
        });
        let auth = tokio::time::timeout(timeout, wait).await.map_err(|_| {
            AuthError::Expired("the ChatGPT login timed out waiting for the browser".into())
        })??;
        exchange(
            &http,
            &endpoints,
            flow,
            &auth,
            &pkce.verifier,
            &redirect_uri,
        )
        .await
    }

    /// Finishes with the callback URL the browser ended on, pasted by you (when the browser can't
    /// reach the local port).
    pub async fn finish_with_pasted_url(self, url: &str) -> Result<Credential, AuthError> {
        let parsed = Url::parse(url.trim())
            .map_err(|_| AuthError::Login("paste the full callback URL from the browser".into()))?;
        let expected = Url::parse(&self.redirect_uri)
            .map_err(|e| AuthError::Login(format!("the redirect URI is invalid: {e}")))?;
        let same_origin = parsed.port_or_known_default() == expected.port_or_known_default()
            && parsed.path() == expected.path()
            && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"));
        if !same_origin {
            return Err(AuthError::Login(format!(
                "the pasted callback URL must start with {}",
                self.redirect_uri
            )));
        }
        let auth = match authorization_from_callback(self.flow, &parsed, &self.state) {
            Ok(auth) => auth,
            Err(CallbackProblem::BrowserError(e) | CallbackProblem::Invalid(e)) => {
                return Err(AuthError::Login(e));
            }
        };
        exchange(
            &self.http,
            &self.endpoints,
            self.flow,
            &auth,
            &self.pkce.verifier,
            &self.redirect_uri,
        )
        .await
    }
}

enum CallbackProblem {
    /// `error=…` from the authorization server: the login is over.
    BrowserError(String),
    /// Not a usable callback: keep waiting.
    Invalid(String),
}

fn authorization_from_callback(
    flow: ChatGptFlow,
    url: &Url,
    expected_state: &str,
) -> Result<Authorization, CallbackProblem> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(error) = param("error") {
        // The error code is the provider's, not a secret; nothing else from the URL is kept.
        return Err(CallbackProblem::BrowserError(format!(
            "ChatGPT authorization failed: {}",
            crate::http::short_code(&error)
        )));
    }
    let code = param("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| CallbackProblem::Invalid("Missing authorization code.".into()))?;
    let state =
        param("state").ok_or_else(|| CallbackProblem::Invalid("Missing OAuth state.".into()))?;
    if state != expected_state {
        return Err(CallbackProblem::Invalid("OAuth state mismatch.".into()));
    }
    let client_id = match flow {
        ChatGptFlow::SignInWithChatGpt => Some(
            param("client_id")
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty())
                .ok_or_else(|| {
                    CallbackProblem::Invalid(
                        "The callback did not contain an issued client ID.".into(),
                    )
                })?,
        ),
        ChatGptFlow::Codex => None,
    };
    Ok(Authorization { code, client_id })
}

async fn exchange(
    http: &reqwest::Client,
    endpoints: &ChatGptEndpoints,
    flow: ChatGptFlow,
    auth: &Authorization,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Credential, AuthError> {
    match flow {
        ChatGptFlow::SignInWithChatGpt => {
            let client_id = auth
                .client_id
                .as_deref()
                .ok_or_else(|| AuthError::Login("no issued client ID".into()))?;
            let token = request_token(
                http,
                endpoints.token_url(flow),
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", client_id),
                    ("code", &auth.code),
                    ("code_verifier", verifier),
                    ("redirect_uri", redirect_uri),
                    ("resource", RESOURCE),
                ],
                "exchange",
            )
            .await?;
            // The ID token is checked for presence only: never decoded, never stored.
            let has_id_token = token
                .get("id_token")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.trim().is_empty());
            if !has_id_token {
                return Err(AuthError::Login(
                    "the token response did not contain an ID token".into(),
                ));
            }
            sign_in_credential(&token, client_id)
        }
        ChatGptFlow::Codex => {
            let token = request_token(
                http,
                endpoints.token_url(flow),
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CODEX_CLIENT_ID),
                    ("code", &auth.code),
                    ("code_verifier", verifier),
                    ("redirect_uri", redirect_uri),
                ],
                "exchange",
            )
            .await?;
            codex_credential(&token)
        }
    }
}

/// Starts the Codex login by device code: for when the browser can't reach port 1455 (another
/// login or the Codex CLI holds it). Adapted from OpenAI Codex
/// `codex-rs/login/src/device_code_auth.rs` (Apache-2.0, Copyright OpenAI). The code lives 15
/// minutes.
pub async fn start_codex_device(
    http: &reqwest::Client,
    endpoints: &ChatGptEndpoints,
) -> Result<DeviceCode, AuthError> {
    let reply = crate::http::post_json(
        http,
        &endpoints.codex_device_usercode_url,
        &serde_json::json!({ "client_id": CODEX_CLIENT_ID }),
        "ChatGPT",
        "device authorization",
    )
    .await?;
    if reply.status == 404 {
        return Err(AuthError::Login(
            "ChatGPT's device login isn't available; use the browser login".into(),
        ));
    }
    if !reply.ok() {
        return Err(reply.failure("ChatGPT", "device authorization"));
    }
    let device_auth_id = reply.str("device_auth_id").ok_or_else(|| {
        AuthError::Login("ChatGPT's device authorization has no device_auth_id".into())
    })?;
    let user_code = reply
        .str("user_code")
        .or_else(|| reply.str("usercode"))
        .ok_or_else(|| {
            AuthError::Login("ChatGPT's device authorization has no user code".into())
        })?;
    // Codex sends the interval as a string.
    let interval = reply
        .body
        .get("interval")
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
        .filter(|n| n.is_finite() && *n > 0.0)
        .map_or(device_code::DEFAULT_INTERVAL, Duration::from_secs_f64);
    Ok(DeviceCode {
        user_code: user_code.trim().to_owned(),
        verification_uri: endpoints.codex_device_verification_url.clone(),
        verification_uri_complete: None,
        expires_in: device_code::DEFAULT_EXPIRY,
        issued_at: tokio::time::Instant::now(),
        interval,
        poll_code: Secret::new(device_auth_id),
    })
}

/// Polls the Codex device login until you approve it, then exchanges the code it yields.
pub async fn finish_codex_device(
    http: &reqwest::Client,
    endpoints: &ChatGptEndpoints,
    device: &DeviceCode,
    cancel: &Cancel,
) -> Result<Credential, AuthError> {
    let (code, verifier) = device_code::poll_until(device, "ChatGPT", cancel, || async {
        let reply = crate::http::post_json(
            http,
            &endpoints.codex_device_token_url,
            &serde_json::json!({
                "device_auth_id": device.poll_code.expose(),
                "user_code": device.user_code,
            }),
            "ChatGPT",
            "device login",
        )
        .await?;
        // Codex answers "not yet" with 403 or 404.
        if matches!(reply.status, 403 | 404) {
            return Ok(Poll::Pending);
        }
        if !reply.ok() {
            return Ok(Poll::Failed(reply.failure("ChatGPT", "device login")));
        }
        match (reply.str("authorization_code"), reply.str("code_verifier")) {
            (Some(code), Some(verifier)) => {
                Ok(Poll::Complete((Secret::new(code), Secret::new(verifier))))
            }
            _ => Ok(Poll::Failed(AuthError::Login(
                "ChatGPT's device login answer has no authorization code".into(),
            ))),
        }
    })
    .await?;
    let auth = Authorization {
        code: code.expose().to_owned(),
        client_id: None,
    };
    let exchange = exchange(
        http,
        endpoints,
        ChatGptFlow::Codex,
        &auth,
        verifier.expose(),
        &endpoints.codex_device_redirect_uri,
    );
    tokio::select! {
        () = cancel.cancelled() => Err(AuthError::Cancelled),
        result = exchange => result,
    }
}

/// Refreshes a ChatGPT credential with its refresh token. Refresh tokens rotate: the caller must
/// `put` the returned credential at once, or the login is lost.
pub async fn refresh(
    endpoints: &ChatGptEndpoints,
    credential: &Credential,
) -> Result<Credential, AuthError> {
    refresh_with(&crate::http::client(), endpoints, credential).await
}

/// As [`refresh`], on a client the caller already has.
pub async fn refresh_with(
    http: &reqwest::Client,
    endpoints: &ChatGptEndpoints,
    credential: &Credential,
) -> Result<Credential, AuthError> {
    let refresh_token = crate::refresh_token_of(credential, "ChatGPT")?;
    if ChatGptFlow::of(credential) == Some(ChatGptFlow::Codex) {
        let token = request_token(
            http,
            endpoints.token_url(ChatGptFlow::Codex),
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.expose()),
                ("client_id", CODEX_CLIENT_ID),
            ],
            "refresh",
        )
        .await?;
        return codex_credential(&token);
    }
    let client_id = issued_client_id(credential).ok_or_else(|| {
        AuthError::Rejected(
            "the stored ChatGPT login has no issued client ID; connect it again in \
             Settings ▸ Accounts"
                .into(),
        )
    })?;
    let token = request_token(
        http,
        endpoints.token_url(ChatGptFlow::SignInWithChatGpt),
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("refresh_token", refresh_token.expose()),
            ("resource", RESOURCE),
        ],
        "refresh",
    )
    .await?;
    sign_in_credential(&token, &client_id)
}

/// The client ID Sign in with ChatGPT issued at login, which every refresh must send.
fn issued_client_id(credential: &Credential) -> Option<String> {
    match credential {
        Credential::OAuth { client_id, .. } => client_id.clone().filter(|c| !c.trim().is_empty()),
        Credential::ApiKey { .. } => None,
    }
}

/// Builds a Sign in with ChatGPT credential from a token response, checking it as Pi does.
fn sign_in_credential(token: &serde_json::Value, client_id: &str) -> Result<Credential, AuthError> {
    let access = required_string(token, "access_token")?;
    let refresh = required_string(token, "refresh_token")?;
    let scope = required_string(token, "scope")?;
    let expires_in = positive_expires_in(token)?;
    if !scope.split_whitespace().any(|s| s == DIRECT_TOKEN_SCOPE) {
        return Err(AuthError::Login(format!(
            "the grant did not include {DIRECT_TOKEN_SCOPE}, so these tokens can't reach the \
             Responses API"
        )));
    }
    Ok(Credential::OAuth {
        access_token: Secret::new(access),
        refresh_token: Some(Secret::new(refresh)),
        expires_at: Some(expiry(expires_in, true)),
        account_id: None,
        client_id: Some(client_id.to_owned()),
    })
}

/// Builds a Codex credential: the account ID comes from the access token's claims.
fn codex_credential(token: &serde_json::Value) -> Result<Credential, AuthError> {
    let access = required_string(token, "access_token")?;
    let refresh = required_string(token, "refresh_token")?;
    let expires_in = positive_expires_in(token)?;
    let account_id = account_id_from_jwt(&access).ok_or_else(|| {
        AuthError::Login("the access token carries no ChatGPT account ID".to_owned())
    })?;
    Ok(Credential::OAuth {
        access_token: Secret::new(access),
        refresh_token: Some(Secret::new(refresh)),
        expires_at: Some(expiry(expires_in, false)),
        account_id: Some(account_id),
        // The Codex login uses the Codex CLI's fixed public client.
        client_id: None,
    })
}

/// The `chatgpt_account_id` claim of a JWT's payload, if it has one. The payload is decoded
/// locally and never logged.
pub fn account_id_from_jwt(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn expiry(expires_in: f64, with_margin: bool) -> Timestamp {
    let mut secs = expires_in;
    if with_margin {
        secs -= EXPIRY_MARGIN.as_secs_f64();
    }
    Timestamp::now()
        .checked_add(SignedDuration::from_secs_f64(secs.max(0.0)))
        .unwrap_or(Timestamp::MAX)
}

fn required_string(token: &serde_json::Value, field: &str) -> Result<String, AuthError> {
    token
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AuthError::Login(format!("the token response has no valid {field}")))
}

fn positive_expires_in(token: &serde_json::Value) -> Result<f64, AuthError> {
    token
        .get("expires_in")
        .and_then(serde_json::Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| AuthError::Login("the token response has no valid expires_in".into()))
}

/// Posts a token request (form-encoded) and returns the JSON object. Errors name the HTTP status
/// and the provider's error code, never the body; refused credentials are
/// [`AuthError::Rejected`].
async fn request_token(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    operation: &str,
) -> Result<serde_json::Value, AuthError> {
    let what = format!("token {operation}");
    let reply = crate::http::post_form(http, url, form, "ChatGPT", &what).await?;
    if !reply.ok() {
        return Err(reply.failure("ChatGPT", &what));
    }
    if reply.body.is_empty() && !reply.json {
        return Err(AuthError::Login(format!(
            "the token {operation} response is not a JSON object"
        )));
    }
    Ok(serde_json::Value::Object(reply.body))
}

/// This installation's stable host ID: `<data dir>/install-id`, a UUID created once with mode
/// 0600. Not a secret (Sign in with ChatGPT sends it as `ext_agent_host_id`), but private.
pub fn install_id(data_dir: &DataDir) -> Result<Uuid, AuthError> {
    install_id_at(&data_dir.root().join("install-id"))
}

fn install_id_at(path: &Path) -> Result<Uuid, AuthError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    match std::fs::read_to_string(path) {
        Ok(text) => {
            return Uuid::parse_str(text.trim())
                .map_err(|_| AuthError::Corrupt(format!("{} is not a UUID", path.display())));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AuthError::Io(e)),
    }
    let id = Uuid::new_v4();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    match created {
        Ok(mut file) => {
            file.write_all(format!("{}\n", id.hyphenated()).as_bytes())?;
            file.sync_all()?;
            Ok(id)
        }
        // Another process created it first: use theirs.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => install_id_at(path),
        Err(e) => Err(AuthError::Io(e)),
    }
}
