//! Kimi Code subscription login: device code.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/kimi-coding.ts` (MIT, Copyright Earendil Works):
//! RFC 8628 against `https://auth.kimi.com` with form-encoded requests and JSON answers, a server
//! error while polling ends the login, and a refresh retried up to three times (1, 2, 4 seconds)
//! on a rate limit, a server error or a dropped connection. A 401, a 403 or `invalid_grant` means
//! the login is gone. The access token goes to `https://api.kimi.com/coding` as a bearer token.

use crate::cancel::Cancel;
use crate::device_code::{self, DeviceCode, Poll};
use crate::http::{self, Reply};
use crate::{AuthError, Credential, Secret, expires_after};
use std::time::Duration;

/// Kimi Code's public client.
pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
/// How many times a failed refresh is retried.
pub const REFRESH_RETRIES: u32 = 3;
/// Tokens count as expired this long before they are.
pub const EXPIRY_SKEW: Duration = Duration::from_mins(3);
const NAME: &str = "Kimi";

/// Where the Kimi login talks. [`Default`] is Kimi's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KimiEndpoints {
    pub device_authorization_url: String,
    pub token_url: String,
}

impl Default for KimiEndpoints {
    fn default() -> Self {
        Self::at_origin("https://auth.kimi.com")
    }
}

impl KimiEndpoints {
    /// Every URL on `origin` (no trailing slash), with the real paths.
    fn at_origin(origin: &str) -> Self {
        let origin = origin.trim_end_matches('/');
        Self {
            device_authorization_url: format!("{origin}/api/oauth/device_authorization"),
            token_url: format!("{origin}/api/oauth/token"),
        }
    }
}

/// Asks Kimi for a device code.
pub async fn start(
    http: &reqwest::Client,
    endpoints: &KimiEndpoints,
) -> Result<DeviceCode, AuthError> {
    let reply = http::post_form(
        http,
        &endpoints.device_authorization_url,
        &[("client_id", CLIENT_ID)],
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
    endpoints: &KimiEndpoints,
    device: &DeviceCode,
    cancel: &Cancel,
) -> Result<Credential, AuthError> {
    device_code::poll_until(device, NAME, cancel, || async {
        let reply = http::post_form(
            http,
            &endpoints.token_url,
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device.poll_code.expose()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
            NAME,
            "device login",
        )
        .await?;
        if reply.status >= 500 {
            return Ok(Poll::Failed(reply.failure(NAME, "device login")));
        }
        if reply.ok() && reply.str("access_token").is_some() {
            return Ok(match credential(&reply) {
                Ok(c) => Poll::Complete(c),
                Err(e) => Poll::Failed(e),
            });
        }
        Ok(device_code::standard_poll_answer(&reply, NAME))
    })
    .await
}

/// Refreshes a Kimi login, retrying a rate limit, a server error or a dropped connection. The
/// caller saves the result at once.
pub async fn refresh(
    http: &reqwest::Client,
    endpoints: &KimiEndpoints,
    credential: &Credential,
) -> Result<Credential, AuthError> {
    let refresh_token = crate::refresh_token_of(credential, NAME)?;
    let mut last = AuthError::Login("the Kimi token refresh failed".into());
    for attempt in 0..=REFRESH_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
        }
        let reply = match http::post_form(
            http,
            &endpoints.token_url,
            &[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.expose()),
            ],
            NAME,
            "token refresh",
        )
        .await
        {
            Ok(reply) => reply,
            Err(e) => {
                last = e;
                continue;
            }
        };
        if reply.ok() {
            return self::credential(&reply);
        }
        let failure = reply.failure(NAME, "token refresh");
        let retryable = reply.status == 429 || reply.status >= 500;
        if matches!(failure, AuthError::Rejected(_)) || !retryable {
            return Err(failure);
        }
        last = failure;
    }
    Err(last)
}

fn credential(reply: &Reply) -> Result<Credential, AuthError> {
    let missing =
        |field: &str| AuthError::Login(format!("Kimi's token response has no valid {field}"));
    let access = reply
        .str("access_token")
        .ok_or_else(|| missing("access_token"))?;
    let refresh = reply
        .str("refresh_token")
        .ok_or_else(|| missing("refresh_token"))?;
    let lifetime =
        http::positive_number(reply, "expires_in").ok_or_else(|| missing("expires_in"))?;
    Ok(Credential::OAuth {
        access_token: Secret::new(access),
        refresh_token: Some(Secret::new(refresh)),
        expires_at: Some(expires_after(lifetime, EXPIRY_SKEW)),
        account_id: None,
        client_id: None,
    })
}
