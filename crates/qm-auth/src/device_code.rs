//! The device-code login (RFC 8628) that xAI, GitHub Copilot and Kimi use: the app shows a code
//! and an address, you enter the code in your browser, and the app polls until the provider says
//! yes, no, or the code expires.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/device-code.ts` (MIT, Copyright Earendil Works):
//! a 5-second default interval, at least 1 second between polls, `slow_down` raises the interval
//! (to the server's value when it sends one, else by 5 seconds), and the code's own expiry ends
//! the wait. Time is Tokio's.
//!
//! The device code is a [`Secret`] (whoever holds it can finish the login); the user code is shown
//! on screen but never logged.

use crate::cancel::Cancel;
use crate::{AuthError, Secret};
use std::fmt;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// RFC 8628 §3.2: the interval when the server sends none.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// Never poll faster than this, whatever the server says.
pub const MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
/// RFC 8628 §3.5: how much `slow_down` adds when the server names no interval.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// How long a code lives when the server doesn't say.
pub const DEFAULT_EXPIRY: Duration = Duration::from_mins(15);

/// A device authorization: what the login screen shows, and what polling needs.
#[derive(Clone)]
pub struct DeviceCode {
    /// The code you type in the browser, e.g. `WDJB-MJHT`.
    pub user_code: String,
    /// Where you type it: shown, and opened in the browser on request.
    pub verification_uri: String,
    /// The address with the code already filled in, when the provider offers one.
    pub verification_uri_complete: Option<String>,
    /// How long the code lives.
    pub expires_in: Duration,
    /// When it was issued.
    pub issued_at: Instant,
    /// How often to poll.
    pub interval: Duration,
    /// The code polling sends: a secret.
    pub(crate) poll_code: Secret,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("verification_uri", &self.verification_uri)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl DeviceCode {
    /// The address to open: the one with the code filled in when there is one.
    pub fn open_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }

    /// How long until the code expires (zero once it has).
    pub fn remaining(&self) -> Duration {
        self.expires_in.saturating_sub(self.issued_at.elapsed())
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.issued_at + self.expires_in
    }
}

/// What one poll found.
#[derive(Debug)]
pub(crate) enum Poll<T> {
    /// Not yet (`authorization_pending`).
    Pending,
    /// Poll more slowly (`slow_down`), at this interval in seconds if the server named one.
    SlowDown(Option<f64>),
    /// Done.
    Complete(T),
    /// The login is over: denied, expired or broken.
    Failed(AuthError),
}

/// Polls until `poll` completes or fails, the code expires, or `cancel` is pressed. The first poll
/// waits one interval, as the providers expect.
pub(crate) async fn poll_until<T, F, Fut>(
    device: &DeviceCode,
    provider: &str,
    cancel: &Cancel,
    mut poll: F,
) -> Result<T, AuthError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll<T>, AuthError>>,
{
    let deadline = device.deadline();
    let mut interval = device.interval.max(MINIMUM_INTERVAL);
    let mut slowed = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = interval.min(deadline - now);
        tokio::select! {
            () = cancel.cancelled() => return Err(AuthError::Cancelled),
            () = tokio::time::sleep(wait) => {}
        }
        if Instant::now() >= deadline {
            break;
        }
        let result = tokio::select! {
            () = cancel.cancelled() => return Err(AuthError::Cancelled),
            result = poll() => result?,
        };
        match result {
            Poll::Complete(value) => return Ok(value),
            Poll::Failed(error) => return Err(error),
            Poll::Pending => {}
            Poll::SlowDown(seconds) => {
                slowed = true;
                interval = match seconds.filter(|s| s.is_finite() && *s > 0.0) {
                    Some(s) => Duration::from_secs_f64(s),
                    None => interval + SLOW_DOWN_STEP,
                }
                .max(MINIMUM_INTERVAL);
            }
        }
    }
    Err(AuthError::Expired(if slowed {
        format!(
            "the {provider} code expired before the login finished (the provider asked to poll \
             more slowly; if this keeps happening, check this Mac's clock)"
        )
    } else {
        format!("the {provider} code expired before the login finished; start again")
    }))
}

/// The standard RFC 8628 answer to a token poll, for providers that follow it.
pub(crate) fn standard_poll_answer<T>(reply: &crate::http::Reply, provider: &str) -> Poll<T> {
    match reply.error_code().as_deref() {
        Some("authorization_pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowDown(crate::http::positive_number(reply, "interval")),
        Some("access_denied" | "authorization_denied") => Poll::Failed(AuthError::Login(format!(
            "the {provider} login was declined in the browser"
        ))),
        Some("expired_token") => Poll::Failed(AuthError::Expired(format!(
            "the {provider} code expired before the login finished; start again"
        ))),
        _ => Poll::Failed(reply.failure(provider, "device login")),
    }
}

/// Builds a [`DeviceCode`] from a standard device authorization response.
pub(crate) fn parse_authorization(
    reply: &crate::http::Reply,
    provider: &str,
) -> Result<DeviceCode, AuthError> {
    let missing = |field: &str| {
        AuthError::Login(format!(
            "{provider}'s device authorization has no valid {field}"
        ))
    };
    let device_code = reply
        .str("device_code")
        .ok_or_else(|| missing("device_code"))?;
    let user_code = reply.str("user_code").ok_or_else(|| missing("user_code"))?;
    let verification_uri = reply
        .str("verification_uri")
        .or_else(|| reply.str("verification_url"))
        .ok_or_else(|| missing("verification_uri"))?;
    let verification_uri = crate::http::trusted_browser_url(verification_uri, provider)?;
    let verification_uri_complete = reply
        .str("verification_uri_complete")
        .map(|u| crate::http::trusted_browser_url(u, provider))
        .transpose()?;
    let interval = crate::http::positive_number(reply, "interval")
        .map_or(DEFAULT_INTERVAL, Duration::from_secs_f64);
    let expires_in = crate::http::positive_number(reply, "expires_in")
        .map_or(DEFAULT_EXPIRY, Duration::from_secs_f64);
    Ok(DeviceCode {
        user_code: user_code.trim().to_owned(),
        verification_uri,
        verification_uri_complete,
        expires_in,
        issued_at: Instant::now(),
        interval,
        poll_code: Secret::new(device_code),
    })
}
