//! The retry policy, after Pi's `packages/ai/src/utils/provider-retry.ts` at the pinned commit
//! (MIT), which mirrors the OpenAI and Anthropic SDKs.
//!
//! Per model call, up to `max_retries` (the spec's, or the provider's 2). Which errors are worth a
//! retry is the adapter's call ([`crate::adapters::ErrorClass`]): network errors and timeouts
//! before any output, 408, 409, 429 (not a usage limit), 5xx, `x-should-retry: true`, never
//! `x-should-retry: false`. The delay is the server's `retry-after-ms`, else `retry-after`
//! (seconds or an HTTP date), else `min(0.5·2ⁱ, 8) s × (1 − jitter·0.25)`; a server delay over
//! 60 s fails the call at once. The loop adds what only it knows: no retry once output for the
//! call was committed, usage limits are account limits, a 401 refreshes the login once.
//!
//! Jitter comes from an RNG seeded once per session.

use crate::adapters::HeaderMap;
use jiff::Timestamp;
use rand::rngs::StdRng;
use rand::{Rng as _, SeedableRng as _};
use std::time::Duration;

/// The longest delay a server may ask for; more fails the call at once.
pub const MAX_SERVER_DELAY: Duration = Duration::from_secs(60);
/// The first back-off step.
const BASE_DELAY: Duration = Duration::from_millis(500);
/// The longest back-off step.
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Decides how long to wait before each retry of one call.
#[derive(Debug)]
pub struct RetryPolicy {
    max_retries: u32,
    rng: StdRng,
}

/// Why no retry will happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoRetry {
    /// Every retry was used.
    Exhausted,
    /// The server asked for a longer wait than [`MAX_SERVER_DELAY`].
    ServerDelayTooLong(Duration),
}

impl RetryPolicy {
    pub fn new(max_retries: u32, seed: u64) -> Self {
        Self {
            max_retries,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// The wait before retry number `retry_index` (0 for the first retry), or why there is none.
    pub fn delay(
        &mut self,
        retry_index: u32,
        headers: Option<&HeaderMap>,
        now: Timestamp,
    ) -> Result<Duration, NoRetry> {
        if retry_index >= self.max_retries {
            return Err(NoRetry::Exhausted);
        }
        if let Some(server) = headers.and_then(|h| server_delay(h, now)) {
            if server > MAX_SERVER_DELAY {
                return Err(NoRetry::ServerDelayTooLong(server));
            }
            return Ok(server);
        }
        let step = BASE_DELAY
            .saturating_mul(2_u32.saturating_pow(retry_index))
            .min(MAX_BACKOFF);
        let jitter: f64 = self.rng.random::<f64>();
        Ok(step.mul_f64(1.0 - jitter * 0.25))
    }
}

/// The delay a server asked for: `retry-after-ms`, else `retry-after` in seconds or as an HTTP
/// date.
pub fn server_delay(headers: &HeaderMap, now: Timestamp) -> Option<Duration> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.parse::<f64>().ok())
        && ms.is_finite()
    {
        return Some(Duration::from_secs_f64(ms.max(0.0) / 1000.0));
    }
    let value = get("retry-after")?;
    if let Ok(secs) = value.parse::<f64>() {
        return secs
            .is_finite()
            .then(|| Duration::from_secs_f64(secs.max(0.0)));
    }
    let date = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
    let wait = date.duration_since(now);
    Some(wait.try_into().unwrap_or(Duration::ZERO))
}
