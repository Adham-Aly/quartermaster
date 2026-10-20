//! GitHub Copilot login: device code.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/github-copilot.ts` (MIT, Copyright Earendil Works).
//! Two tokens: the device-code login yields a long-lived **GitHub** token (kept as the
//! credential's refresh token), which is exchanged for a short-lived **Copilot** token (the access
//! token, about 30 minutes). "Refreshing" exchanges the GitHub token again. The Copilot token
//! names the API host it works with (`proxy-ep=…`), which [`api_base_url`] turns into the base URL.
//!
//! Copilot borrows VS Code Copilot Chat's public client ID and editor headers, as every
//! third-party Copilot client does; that borrowed login is kept isolated so it breaking touches
//! nothing else. GitHub Enterprise isn't in the MVP.

use crate::cancel::Cancel;
use crate::device_code::{self, DeviceCode, Poll};
use crate::http;
use crate::{AuthError, Credential, Secret};
use jiff::{SignedDuration, Timestamp};
use std::time::Duration;

/// VS Code Copilot Chat's public OAuth client.
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
/// The scope the device login asks for.
pub const SCOPE: &str = "read:user";
/// The editor headers Copilot expects on the token exchange and on every API request.
pub const HEADERS: &[(&str, &str)] = &[
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];
/// The API base when the token names none.
pub const DEFAULT_API_BASE: &str = "https://api.individual.githubcopilot.com";
/// Copilot tokens count as expired this long before they are.
pub const EXPIRY_SKEW: Duration = Duration::from_mins(5);
const NAME: &str = "GitHub";

/// Where the Copilot login talks. [`Default`] is GitHub's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopilotEndpoints {
    pub device_code_url: String,
    pub access_token_url: String,
    /// Exchanges the GitHub token for a Copilot token.
    pub copilot_token_url: String,
}

impl Default for CopilotEndpoints {
    fn default() -> Self {
        Self {
            device_code_url: "https://github.com/login/device/code".into(),
            access_token_url: "https://github.com/login/oauth/access_token".into(),
            copilot_token_url: "https://api.github.com/copilot_internal/v2/token".into(),
        }
    }
}

/// Asks GitHub for a device code.
pub async fn start(
    http: &reqwest::Client,
    endpoints: &CopilotEndpoints,
) -> Result<DeviceCode, AuthError> {
    let reply = http::post_form(
        http,
        &endpoints.device_code_url,
        &[("client_id", CLIENT_ID), ("scope", SCOPE)],
        NAME,
        "device authorization",
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure(NAME, "device authorization"));
    }
    device_code::parse_authorization(&reply, NAME)
}

/// Polls until you approve the code, then gets the first Copilot token.
pub async fn finish(
    http: &reqwest::Client,
    endpoints: &CopilotEndpoints,
    device: &DeviceCode,
    cancel: &Cancel,
) -> Result<Credential, AuthError> {
    let github_token = device_code::poll_until(device, NAME, cancel, || async {
        let reply = http::post_form(
            http,
            &endpoints.access_token_url,
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device.poll_code.expose()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
            NAME,
            "device login",
        )
        .await?;
        // GitHub answers a pending poll with 200 and an `error` field.
        if reply.ok()
            && let Some(token) = reply.str("access_token")
        {
            return Ok(Poll::Complete(Secret::new(token)));
        }
        Ok(device_code::standard_poll_answer(&reply, NAME))
    })
    .await?;
    tokio::select! {
        () = cancel.cancelled() => Err(AuthError::Cancelled),
        result = exchange(http, endpoints, github_token) => result,
    }
}

/// Gets a fresh Copilot token with the stored GitHub token. The caller saves the result at once.
pub async fn refresh(
    http: &reqwest::Client,
    endpoints: &CopilotEndpoints,
    credential: &Credential,
) -> Result<Credential, AuthError> {
    let github_token = crate::refresh_token_of(credential, "GitHub Copilot")?.clone();
    exchange(http, endpoints, github_token).await
}

async fn exchange(
    http: &reqwest::Client,
    endpoints: &CopilotEndpoints,
    github_token: Secret,
) -> Result<Credential, AuthError> {
    let mut request = http
        .get(&endpoints.copilot_token_url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {}", github_token.expose()));
    for (name, value) in HEADERS {
        request = request.header(*name, *value);
    }
    let reply = http::send(request, "GitHub Copilot", "token exchange").await?;
    if !reply.ok() {
        let failure = reply.failure("GitHub Copilot", "token exchange");
        // 404 here means this GitHub account has no Copilot subscription.
        return Err(if reply.status == 404 {
            AuthError::Rejected(
                "this GitHub account has no Copilot subscription (GitHub answered 404)".into(),
            )
        } else {
            failure
        });
    }
    let token = reply.str("token").ok_or_else(|| {
        AuthError::Login("GitHub Copilot's token response has no token".to_owned())
    })?;
    let expires_at = reply
        .body
        .get("expires_at")
        .and_then(serde_json::Value::as_i64)
        .and_then(|secs| Timestamp::from_second(secs).ok())
        .ok_or_else(|| {
            AuthError::Login("GitHub Copilot's token response has no valid expires_at".to_owned())
        })?;
    let skew = SignedDuration::try_from(EXPIRY_SKEW).unwrap_or_default();
    Ok(Credential::OAuth {
        access_token: Secret::new(token),
        refresh_token: Some(github_token),
        expires_at: Some(expires_at.checked_sub(skew).unwrap_or(expires_at)),
        account_id: None,
        client_id: None,
    })
}

/// The Copilot API base URL a Copilot token works with: its `proxy-ep=proxy.<host>` field as
/// `https://api.<host>`, or [`DEFAULT_API_BASE`]. Reads the token locally; never logs it.
pub fn api_base_url(access_token: &Secret) -> String {
    access_token
        .expose()
        .split(';')
        .find_map(|field| field.strip_prefix("proxy-ep="))
        .map(str::trim)
        .filter(|host| {
            !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        })
        .map_or_else(
            || DEFAULT_API_BASE.to_owned(),
            |host| match host.strip_prefix("proxy.") {
                Some(rest) => format!("https://api.{rest}"),
                None => format!("https://{host}"),
            },
        )
}
