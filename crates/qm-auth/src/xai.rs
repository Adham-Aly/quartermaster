//! Grok (xAI) subscription login: device code.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/xai.ts` (MIT, Copyright Earendil Works): the
//! client, scopes and endpoints, form-encoded requests, a refresh that may keep the old refresh
//! token when xAI doesn't rotate it, and a default lifetime of an hour when the response names
//! none. The access token is sent as a bearer token to `https://api.x.ai/v1`.

use crate::cancel::Cancel;
use crate::device_code::{self, DeviceCode, Poll};
use crate::http::{self, Reply};
use crate::{AuthError, Credential, Secret, expires_after};
use std::time::Duration;

/// xAI's public client for subscription logins.
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// The scopes the login asks for.
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Tokens count as expired this long before they are.
pub const EXPIRY_SKEW: Duration = Duration::from_mins(5);
/// The lifetime assumed when a token response names none.
pub const DEFAULT_LIFETIME: Duration = Duration::from_hours(1);
const NAME: &str = "xAI";

/// Where the xAI login talks. [`Default`] is xAI's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XaiEndpoints {
    pub device_code_url: String,
    pub token_url: String,
}

impl Default for XaiEndpoints {
    fn default() -> Self {
        Self::at_origin("https://auth.x.ai")
    }
}

impl XaiEndpoints {
    /// Every URL on `origin` (no trailing slash), with the real paths.
    fn at_origin(origin: &str) -> Self {
        let origin = origin.trim_end_matches('/');
        Self {
            device_code_url: format!("{origin}/oauth2/device/code"),
            token_url: format!("{origin}/oauth2/token"),
        }
    }
}

/// Asks xAI for a device code.
pub async fn start(
    http: &reqwest::Client,
    endpoints: &XaiEndpoints,
) -> Result<DeviceCode, AuthError> {
    let reply = http::post_form(
        http,
        &endpoints.device_code_url,
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "quartermaster"),
        ],
        NAME,
        "device authorization",
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure(NAME, "device authorization"));
    }
    device_code::parse_authorization(&reply, NAME)
}

/// Polls until you approve the code in the browser.
pub async fn finish(
    http: &reqwest::Client,
    endpoints: &XaiEndpoints,
    device: &DeviceCode,
    cancel: &Cancel,
) -> Result<Credential, AuthError> {
    device_code::poll_until(device, NAME, cancel, || async {
        let reply = http::post_form(
            http,
            &endpoints.token_url,
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device.poll_code.expose()),
            ],
            NAME,
            "device login",
        )
        .await?;
        if reply.ok() {
            return Ok(match credential(&reply, None) {
                Ok(c) => Poll::Complete(c),
                Err(e) => Poll::Failed(e),
            });
        }
        Ok(device_code::standard_poll_answer(&reply, NAME))
    })
    .await
}

/// Refreshes an xAI login. The caller saves the result at once.
pub async fn refresh(
    http: &reqwest::Client,
    endpoints: &XaiEndpoints,
    credential: &Credential,
) -> Result<Credential, AuthError> {
    let refresh_token = crate::refresh_token_of(credential, NAME)?;
    let reply = http::post_form(
        http,
        &endpoints.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token.expose()),
        ],
        NAME,
        "token refresh",
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure(NAME, "token refresh"));
    }
    self::credential(&reply, Some(refresh_token))
}

fn credential(reply: &Reply, previous_refresh: Option<&Secret>) -> Result<Credential, AuthError> {
    let missing =
        |field: &str| AuthError::Login(format!("xAI's token response has no valid {field}"));
    let access = reply
        .str("access_token")
        .ok_or_else(|| missing("access_token"))?;
    // xAI may leave the refresh token out of a refresh when it doesn't rotate it.
    let refresh = match (reply.str("refresh_token"), previous_refresh) {
        (Some(new), _) => Secret::new(new),
        (None, Some(old)) => old.clone(),
        (None, None) => return Err(missing("refresh_token")),
    };
    let lifetime = match reply.body.get("expires_in") {
        None => DEFAULT_LIFETIME.as_secs_f64(),
        Some(_) => {
            http::positive_number(reply, "expires_in").ok_or_else(|| missing("expires_in"))?
        }
    };
    Ok(Credential::OAuth {
        access_token: Secret::new(access),
        refresh_token: Some(refresh),
        expires_at: Some(expires_after(lifetime, EXPIRY_SKEW)),
        account_id: None,
        client_id: None,
    })
}
