//! Display-safe quota snapshots for connected accounts.
//!
//! ChatGPT's wham and Copilot's internal-user endpoints are private provider APIs and may
//! change. OpenRouter exposes a public key-budget API (not the whole account's balance).
//! No Claude credential is ever accessed. Other providers explicitly return
//! [`QuotaError::Unsupported`]; inference rate-limit headers are not account quota APIs.
//! Only allow-listed numeric fields and parsed timestamps escape this module, never provider
//! strings, response bodies, identifiers, tokens, or underlying authentication errors.

use crate::{Credential, CredentialRef, Secret, TokenKeeper};
use jiff::Timestamp;
use qm_types::domain::Provider;
use serde_json::Value;
use std::{fmt, sync::Arc};

/// Trusted destinations for quota requests. Override only for a trusted service or local tests:
/// these URLs receive credentials. HTTPS is required except on loopback; redirects are refused.
#[derive(Clone, Debug)]
pub struct QuotaEndpoints {
    pub chatgpt: String,
    pub copilot: String,
    pub openrouter: String,
}

impl Default for QuotaEndpoints {
    fn default() -> Self {
        Self {
            chatgpt: "https://chatgpt.com/backend-api/wham/usage".into(),
            copilot: "https://api.github.com/copilot_internal/user".into(),
            openrouter: "https://openrouter.ai/api/v1/key".into(),
        }
    }
}

/// Fixed, safe messages suitable for the UI. No underlying provider error is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QuotaError {
    #[error("Quota lookup is not supported for this provider or credential type.")]
    Unsupported,
    #[error("Credentials are unavailable; check the account in Settings.")]
    Credentials,
    #[error("Quota access was refused; check the account in Settings.")]
    Refused,
    #[error("Quota requests are rate limited; try again later.")]
    RateLimited,
    #[error("Quota service is unavailable; try again later.")]
    Unavailable,
    #[error("Quota service returned an unrecognized response.")]
    InvalidResponse,
}

/// An allow-listed quota category, never a provider-supplied display string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuotaKind {
    Primary,
    Secondary,
    CodeReview,
    Chat,
    Completions,
    PremiumRequests,
    /// OpenRouter's per-key budget; not a claim about the account wallet.
    KeyBudget,
}

impl fmt::Display for QuotaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Primary => "Primary",
            Self::Secondary => "Secondary",
            Self::CodeReview => "Code review",
            Self::Chat => "Chat",
            Self::Completions => "Completions",
            Self::PremiumRequests => "Premium requests",
            Self::KeyBudget => "Key budget",
        })
    }
}

/// One provider-reported bucket. Missing information stays unknown, not zero.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct QuotaBucket {
    pub kind: QuotaKind,
    pub remaining_percent: Option<f64>,
    pub remaining: Option<u64>,
    pub entitlement: Option<u64>,
    /// OpenRouter's remaining key budget in USD, not an account-wide credit balance.
    pub remaining_usd: Option<f64>,
    /// Unlimited applies only to this bucket (for `KeyBudget`, only the key's spending cap).
    pub unlimited: bool,
    pub resets_at: Option<Timestamp>,
    /// A provider-reported calendar date when no reset time or timezone is specified.
    pub resets_on: Option<jiff::civil::Date>,
    pub window_seconds: Option<u64>,
}

impl fmt::Display for QuotaBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if matches!(self.kind, QuotaKind::Primary | QuotaKind::Secondary)
            && let Some(seconds) = self.window_seconds.filter(|seconds| *seconds > 0)
        {
            let (amount, unit) = if seconds.is_multiple_of(86_400) {
                (seconds / 86_400, "day")
            } else if seconds.is_multiple_of(3_600) {
                (seconds / 3_600, "hour")
            } else if seconds.is_multiple_of(60) {
                (seconds / 60, "minute")
            } else {
                (seconds, "second")
            };
            write!(f, "{amount}-{unit}: ")?;
        } else {
            write!(f, "{}: ", self.kind)?;
        }
        if self.unlimited {
            f.write_str("unlimited")?;
        } else if let Some(usd) = self.remaining_usd {
            write!(f, "${usd:.2} remaining")?;
        } else if let Some(percent) = self.remaining_percent {
            write!(f, "{percent:.0}% remaining")?;
        } else if let Some(remaining) = self.remaining {
            write!(f, "{remaining} remaining")?;
        } else {
            f.write_str("remaining unknown")?;
        }
        if let Some(reset) = self.resets_at {
            write!(f, "; resets {reset}")?;
        } else if let Some(reset) = self.resets_on {
            write!(f, "; resets {reset}")?;
        }
        Ok(())
    }
}

/// A point-in-time snapshot, not a guarantee that the next request can run.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct QuotaSummary {
    pub buckets: Vec<QuotaBucket>,
}

/// Fetches quotas using the same keeper (and refresh locks) as inference.
pub struct Quotas {
    tokens: Arc<TokenKeeper>,
    endpoints: QuotaEndpoints,
    http: reqwest::Client,
}

impl fmt::Debug for Quotas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Quotas").finish_non_exhaustive()
    }
}

impl Quotas {
    pub fn new(tokens: Arc<TokenKeeper>) -> Self {
        Self::with_endpoints(tokens, QuotaEndpoints::default())
    }

    pub fn with_endpoints(tokens: Arc<TokenKeeper>, endpoints: QuotaEndpoints) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("quartermaster/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(crate::http::CONNECT_TIMEOUT)
            .timeout(crate::http::REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("quota HTTP client configuration is valid");
        Self {
            tokens,
            endpoints,
            http,
        }
    }

    /// Unsupported providers (especially Claude) return before even reading the store.
    /// A ChatGPT 401 retries once via `TokenKeeper::fresh`; Copilot uses the stored GitHub
    /// refresh token, not its inference token, so re-exchanging the latter cannot fix a 401.
    /// Quota-only failures do not mark inference credentials unhealthy.
    pub async fn fetch(
        &self,
        provider: Provider,
        key: &CredentialRef,
    ) -> Result<QuotaSummary, QuotaError> {
        let endpoint = match provider {
            Provider::ChatGpt => &self.endpoints.chatgpt,
            Provider::Copilot => &self.endpoints.copilot,
            Provider::OpenRouter => &self.endpoints.openrouter,
            _ => return Err(QuotaError::Unsupported),
        };
        validate_endpoint(endpoint)?;
        // Copilot quota uses the long-lived GitHub token, independently of whether the
        // short-lived inference token can still be exchanged successfully.
        let mut credential = if provider == Provider::Copilot {
            self.tokens
                .stored(key)
                .map_err(|_| QuotaError::Credentials)?
                .ok_or(QuotaError::Credentials)?
        } else {
            self.tokens
                .fresh(provider, key, None)
                .await
                .map_err(|_| QuotaError::Credentials)?
        };
        for attempt in 0..2 {
            let (token, account_id) = match (&credential, provider) {
                (Credential::ApiKey { key }, Provider::OpenRouter) => (key, None),
                (
                    Credential::OAuth {
                        access_token,
                        account_id,
                        ..
                    },
                    Provider::ChatGpt,
                ) => (access_token, account_id.as_deref()),
                (Credential::OAuth { refresh_token, .. }, Provider::Copilot) => {
                    (refresh_token.as_ref().ok_or(QuotaError::Credentials)?, None)
                }
                _ => return Err(QuotaError::Unsupported),
            };
            let response = self.request(provider, endpoint, token, account_id).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && provider == Provider::ChatGpt
                && attempt == 0
            {
                credential = self
                    .tokens
                    .fresh(provider, key, Some(token))
                    .await
                    .map_err(|_| QuotaError::Credentials)?;
                continue;
            }
            match response.status().as_u16() {
                200..=299 => {}
                401 | 403 => return Err(QuotaError::Refused),
                429 => return Err(QuotaError::RateLimited),
                _ => return Err(QuotaError::Unavailable),
            }
            let value = read_json(response).await?;
            return match provider {
                Provider::ChatGpt => parse_chatgpt(&value),
                Provider::Copilot => parse_copilot(&value),
                Provider::OpenRouter => parse_openrouter(&value),
                _ => Err(QuotaError::Unsupported),
            };
        }
        Err(QuotaError::Refused)
    }

    async fn request(
        &self,
        provider: Provider,
        endpoint: &str,
        token: &Secret,
        account_id: Option<&str>,
    ) -> Result<reqwest::Response, QuotaError> {
        let mut request = self
            .http
            .get(endpoint)
            .bearer_auth(token.expose())
            .header("accept", "application/json");
        if provider == Provider::ChatGpt {
            if let Some(id) = account_id {
                request = request.header("chatgpt-account-id", id);
            }
        } else if provider == Provider::Copilot {
            for (name, value) in crate::copilot::HEADERS {
                request = request.header(*name, *value);
            }
        }
        request.send().await.map_err(|_| QuotaError::Unavailable)
    }
}

fn validate_endpoint(endpoint: &str) -> Result<(), QuotaError> {
    let url = url::Url::parse(endpoint).map_err(|_| QuotaError::Unavailable)?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.username().is_empty()
        && url.password().is_none()
    {
        Ok(())
    } else {
        Err(QuotaError::Unavailable)
    }
}

async fn read_json(mut response: reqwest::Response) -> Result<Value, QuotaError> {
    const MAX_BODY: usize = 1024 * 1024;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| QuotaError::Unavailable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(QuotaError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| QuotaError::InvalidResponse)
}

fn percent(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
}

fn bucket(kind: QuotaKind) -> QuotaBucket {
    QuotaBucket {
        kind,
        remaining_percent: None,
        remaining: None,
        entitlement: None,
        remaining_usd: None,
        unlimited: false,
        resets_at: None,
        resets_on: None,
        window_seconds: None,
    }
}

fn parse_chatgpt(value: &Value) -> Result<QuotaSummary, QuotaError> {
    let mut buckets = Vec::new();
    for (group, field, kind) in [
        ("rate_limit", "primary_window", QuotaKind::Primary),
        ("rate_limit", "secondary_window", QuotaKind::Secondary),
        (
            "code_review_rate_limit",
            "primary_window",
            QuotaKind::CodeReview,
        ),
    ] {
        let window = &value[group][field];
        if window.is_null() {
            continue;
        }
        let mut b = bucket(kind);
        b.remaining_percent =
            Some(100.0 - percent(&window["used_percent"]).ok_or(QuotaError::InvalidResponse)?);
        b.resets_at = window["reset_at"]
            .as_i64()
            .and_then(|s| Timestamp::from_second(s).ok());
        b.window_seconds = window["limit_window_seconds"].as_u64();
        buckets.push(b);
    }
    summary(buckets)
}

fn parse_copilot(value: &Value) -> Result<QuotaSummary, QuotaError> {
    // Only parse an absolute timestamp: a date-only reset must not invent a time or timezone.
    let reset = value["quota_reset_date"]
        .as_str()
        .and_then(|s| s.parse::<Timestamp>().ok());
    let reset_date = value["quota_reset_date"]
        .as_str()
        .filter(|s| s.len() == 10)
        .and_then(|s| s.parse::<jiff::civil::Date>().ok());
    let mut buckets = Vec::new();
    for (field, kind) in [
        ("chat", QuotaKind::Chat),
        ("completions", QuotaKind::Completions),
        ("premium_interactions", QuotaKind::PremiumRequests),
    ] {
        let snapshot = &value["quota_snapshots"][field];
        if snapshot.is_null() {
            continue;
        }
        let mut b = bucket(kind);
        b.unlimited = snapshot["unlimited"].as_bool().unwrap_or(false);
        b.remaining_percent = percent(&snapshot["percent_remaining"]);
        b.remaining = snapshot["remaining"].as_u64();
        b.entitlement = snapshot["entitlement"].as_u64();
        b.resets_at = reset;
        b.resets_on = reset_date;
        if !b.unlimited && b.remaining_percent.is_none() && b.remaining.is_none() {
            return Err(QuotaError::InvalidResponse);
        }
        buckets.push(b);
    }
    summary(buckets)
}

fn parse_openrouter(value: &Value) -> Result<QuotaSummary, QuotaError> {
    let data = value["data"]
        .as_object()
        .ok_or(QuotaError::InvalidResponse)?;
    let limit = data.get("limit").ok_or(QuotaError::InvalidResponse)?;
    let mut b = bucket(QuotaKind::KeyBudget);
    if limit.is_null() {
        // No per-key spending cap does not mean unlimited account credit.
        b.unlimited = true;
    } else {
        let limit = limit
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or(QuotaError::InvalidResponse)?;
        let remaining = data
            .get("limit_remaining")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or(QuotaError::InvalidResponse)?;
        b.remaining_usd = Some(remaining);
        if limit > 0.0 {
            b.remaining_percent = Some((remaining / limit * 100.0).clamp(0.0, 100.0));
        }
    }
    summary(vec![b])
}

fn summary(buckets: Vec<QuotaBucket>) -> Result<QuotaSummary, QuotaError> {
    if buckets.is_empty() {
        Err(QuotaError::InvalidResponse)
    } else {
        Ok(QuotaSummary { buckets })
    }
}

#[cfg(test)]
mod tests;
