//! Display-safe quota snapshots for connected accounts.
//!
//! ChatGPT's wham and Copilot's internal-user endpoints are private provider APIs and may
//! change. OpenRouter exposes a public key-budget API (not the whole account's balance).
//!
//! **Prepaid balances** ([`BalanceSource`]) are read for the API keys whose provider lets that
//! same key read the account's top-up balance: DeepSeek and Moonshot (Kimi's platform), both
//! connected as custom endpoints and recognised by the endpoint's host, the host that key already
//! goes to. The request goes to the source's fixed URL, never one built from the endpoint. Every
//! other API-key provider the app supports has no such API for an inference key: Anthropic,
//! OpenAI and Gemini have none, and OpenRouter's account credits and xAI's prepaid balance need a
//! separate management key, so OpenRouter reports only the key's own budget.
//!
//! No Claude credential is ever accessed. Accounts with no quota source return
//! [`QuotaError::NoCheck`] with the reason ([`no_check_for`] decides, with no I/O); a Sign in with
//! ChatGPT login is one, since `wham/usage` takes only the Codex login, so a quota check never
//! sends or refreshes its token. Inference rate-limit headers are not account quota APIs.
//! Only allow-listed numeric fields and parsed timestamps escape this module, never provider
//! strings, response bodies, identifiers, tokens, or underlying authentication errors.

use crate::chatgpt::ChatGptFlow;
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
    /// [`BalanceSource::DeepSeek`]'s balance endpoint.
    pub deepseek: String,
    /// [`BalanceSource::Moonshot`]'s balance endpoint.
    pub moonshot: String,
    /// [`BalanceSource::MoonshotChina`]'s balance endpoint.
    pub moonshot_cn: String,
}

impl QuotaEndpoints {
    /// Every endpoint at one loopback URL, for a local stand-in service in tests and developer
    /// runs; `None` for any URL off this machine.
    pub fn local(url: &str) -> Option<Self> {
        let parsed = url::Url::parse(url).ok()?;
        let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        (loopback && matches!(parsed.scheme(), "http" | "https")).then(|| Self {
            chatgpt: url.into(),
            copilot: url.into(),
            openrouter: url.into(),
            deepseek: url.into(),
            moonshot: url.into(),
            moonshot_cn: url.into(),
        })
    }
}

impl Default for QuotaEndpoints {
    fn default() -> Self {
        Self {
            chatgpt: "https://chatgpt.com/backend-api/wham/usage".into(),
            copilot: "https://api.github.com/copilot_internal/user".into(),
            openrouter: "https://openrouter.ai/api/v1/key".into(),
            deepseek: "https://api.deepseek.com/user/balance".into(),
            moonshot: "https://api.moonshot.ai/v1/users/me/balance".into(),
            moonshot_cn: "https://api.moonshot.cn/v1/users/me/balance".into(),
        }
    }
}

/// Fixed, safe messages suitable for the UI. No underlying provider error is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QuotaError {
    /// The account has no quota check: a neutral fact, not a failure.
    #[error("{0}")]
    NoCheck(NoQuotaCheck),
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

/// Why an account has no provider quota check. Not a failure: nothing to fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoQuotaCheck {
    /// The app never reads Claude's login; Claude reports usage during its turns.
    Claude,
    /// A Sign in with ChatGPT token is for the OpenAI API; ChatGPT's usage endpoint takes only
    /// the Codex login.
    SignInWithChatGpt,
    /// The provider (or this kind of credential) has no quota API.
    NoSource,
}

/// Written to follow "Provider quota: ".
impl fmt::Display for NoQuotaCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Claude => "not offered; Claude reports its usage during turns",
            Self::SignInWithChatGpt => "not offered for Sign in with ChatGPT",
            Self::NoSource => "not offered by this provider",
        })
    }
}

/// Why an account with this provider and base URL has no quota check, or `None` when
/// [`Quotas::fetch`] would ask one. Reads no credential and makes no request. A ChatGPT account
/// gets `None` here; its stored login decides ([`NoQuotaCheck::SignInWithChatGpt`]).
pub fn no_check_for(provider: Provider, base_url: Option<&str>) -> Option<NoQuotaCheck> {
    match provider {
        Provider::Claude => Some(NoQuotaCheck::Claude),
        Provider::ChatGpt | Provider::Copilot | Provider::OpenRouter => None,
        _ if BalanceSource::of(provider, base_url).is_some() => None,
        _ => Some(NoQuotaCheck::NoSource),
    }
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
    /// A prepaid account balance ([`BalanceSource`]), in [`QuotaBucket::balance`].
    Balance,
}

/// An allow-listed currency for a balance; any other currency is left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Currency {
    Usd,
    Cny,
}

impl Currency {
    /// The app's own name for it, for storing: `usd`, `cny`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Usd => "usd",
            Self::Cny => "cny",
        }
    }
}

/// An amount of money left, never negative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Money {
    pub amount: f64,
    pub currency: Currency,
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.currency {
            Currency::Usd => write!(f, "${:.2}", self.amount),
            Currency::Cny => write!(f, "CN¥{:.2}", self.amount),
        }
    }
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
            Self::Balance => "Balance",
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
    /// A prepaid balance left (for `Balance`); it has no entitlement, so no percent either.
    pub balance: Option<Money>,
    /// Unlimited applies only to this bucket (for `KeyBudget`, only the key's spending cap).
    pub unlimited: bool,
    pub resets_at: Option<Timestamp>,
    /// A provider-reported calendar date when no reset time or timezone is specified.
    pub resets_on: Option<jiff::civil::Date>,
    pub window_seconds: Option<u64>,
}

impl QuotaBucket {
    /// A bucket of this kind with nothing known yet.
    pub fn new(kind: QuotaKind) -> Self {
        Self {
            kind,
            remaining_percent: None,
            remaining: None,
            entitlement: None,
            remaining_usd: None,
            balance: None,
            unlimited: false,
            resets_at: None,
            resets_on: None,
            window_seconds: None,
        }
    }
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
        } else if let Some(money) = self.balance {
            write!(f, "{money}")?;
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

impl QuotaSummary {
    pub fn new(buckets: Vec<QuotaBucket>) -> Self {
        Self { buckets }
    }
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

    /// Accounts with no quota check ([`no_check_for`], especially Claude) return
    /// [`QuotaError::NoCheck`] before even reading the store. An API key's prepaid balance is
    /// read where [`BalanceSource::of`] finds one for its `base_url`.
    /// A Sign in with ChatGPT login returns [`NoQuotaCheck::SignInWithChatGpt`] after reading
    /// the stored credential, never refreshing or sending it. A Codex login's 401 retries once
    /// via `TokenKeeper::fresh`; Copilot uses the stored GitHub
    /// refresh token, not its inference token, so re-exchanging the latter cannot fix a 401.
    /// Quota-only failures do not mark inference credentials unhealthy.
    pub async fn fetch(
        &self,
        provider: Provider,
        key: &CredentialRef,
        base_url: Option<&str>,
    ) -> Result<QuotaSummary, QuotaError> {
        if let Some(why) = no_check_for(provider, base_url) {
            return Err(QuotaError::NoCheck(why));
        }
        let balance = BalanceSource::of(provider, base_url);
        let endpoint = match (provider, balance) {
            (_, Some(source)) => source.endpoint(&self.endpoints),
            (Provider::ChatGpt, _) => &self.endpoints.chatgpt,
            (Provider::Copilot, _) => &self.endpoints.copilot,
            (Provider::OpenRouter, _) => &self.endpoints.openrouter,
            _ => return Err(QuotaError::NoCheck(NoQuotaCheck::NoSource)),
        };
        validate_endpoint(endpoint)?;
        let stored = || {
            self.tokens
                .stored(key)
                .map_err(|_| QuotaError::Credentials)?
                .ok_or(QuotaError::Credentials)
        };
        // `wham/usage` takes only the Codex login. A Sign in with ChatGPT token is for the
        // OpenAI API: sending it only earns a 401 and a forced refresh, so it is never sent.
        if provider == Provider::ChatGpt
            && ChatGptFlow::of(&stored()?) == Some(ChatGptFlow::SignInWithChatGpt)
        {
            return Err(QuotaError::NoCheck(NoQuotaCheck::SignInWithChatGpt));
        }
        // Copilot quota uses the long-lived GitHub token, independently of whether the
        // short-lived inference token can still be exchanged successfully.
        let mut credential = if provider == Provider::Copilot {
            stored()?
        } else {
            self.tokens
                .fresh(provider, key, None)
                .await
                .map_err(|_| QuotaError::Credentials)?
        };
        for attempt in 0..2 {
            let (token, account_id) = match (&credential, provider) {
                (Credential::ApiKey { key }, _) if balance.is_some() => (key, None),
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
                _ => return Err(QuotaError::NoCheck(NoQuotaCheck::NoSource)),
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
            let status = response.status().as_u16();
            if !(200..=299).contains(&status) {
                // Only the provider and the numeric status: never a URL, token, body or ID.
                tracing::info!(%provider, status, "a quota check was refused or failed");
            }
            match status {
                200..=299 => {}
                401 | 403 => return Err(QuotaError::Refused),
                429 => return Err(QuotaError::RateLimited),
                _ => return Err(QuotaError::Unavailable),
            }
            let value = read_json(response).await?;
            return match (provider, balance) {
                (_, Some(source)) => source.parse(&value),
                (Provider::ChatGpt, _) => parse_chatgpt(&value),
                (Provider::Copilot, _) => parse_copilot(&value),
                (Provider::OpenRouter, _) => parse_openrouter(&value),
                _ => Err(QuotaError::NoCheck(NoQuotaCheck::NoSource)),
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

/// Where an API key can read its account's prepaid (top-up) balance with the key itself. Each
/// is a custom endpoint at the provider's own API host; see the module docs for the providers
/// that have no such API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BalanceSource {
    /// `https://api.deepseek.com`: `GET /user/balance`, in US dollars or yuan.
    DeepSeek,
    /// `https://api.moonshot.ai` (Kimi's global platform): `GET /v1/users/me/balance`, in US
    /// dollars.
    Moonshot,
    /// `https://api.moonshot.cn` (Kimi's platform in China): the same, in yuan.
    MoonshotChina,
}

impl BalanceSource {
    /// The balance an account can read: only a custom endpoint at one of these hosts, over HTTPS
    /// on the default port with no credentials in the URL (any path).
    pub fn of(provider: Provider, base_url: Option<&str>) -> Option<Self> {
        if provider != Provider::Custom {
            return None;
        }
        let url = url::Url::parse(base_url?).ok()?;
        if url.scheme() != "https"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        match url.host_str()? {
            "api.deepseek.com" => Some(Self::DeepSeek),
            "api.moonshot.ai" => Some(Self::Moonshot),
            "api.moonshot.cn" => Some(Self::MoonshotChina),
            _ => None,
        }
    }

    fn endpoint(self, endpoints: &QuotaEndpoints) -> &str {
        match self {
            Self::DeepSeek => &endpoints.deepseek,
            Self::Moonshot => &endpoints.moonshot,
            Self::MoonshotChina => &endpoints.moonshot_cn,
        }
    }

    fn parse(self, value: &Value) -> Result<QuotaSummary, QuotaError> {
        match self {
            Self::DeepSeek => parse_deepseek(value),
            Self::Moonshot => parse_moonshot(value, Currency::Usd),
            Self::MoonshotChina => parse_moonshot(value, Currency::Cny),
        }
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
    QuotaBucket::new(kind)
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

/// DeepSeek's balance: one bucket per allow-listed currency. Amounts come as decimal strings
/// (numbers are accepted too); a negative balance counts as none left.
fn parse_deepseek(value: &Value) -> Result<QuotaSummary, QuotaError> {
    let infos = value["balance_infos"]
        .as_array()
        .ok_or(QuotaError::InvalidResponse)?;
    let mut buckets = Vec::new();
    for info in infos {
        let currency = match info["currency"].as_str() {
            Some("USD") => Currency::Usd,
            Some("CNY") => Currency::Cny,
            _ => continue,
        };
        let total = &info["total_balance"];
        let amount = total
            .as_str()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .or_else(|| total.as_f64())
            .filter(|n| n.is_finite())
            .ok_or(QuotaError::InvalidResponse)?;
        if buckets
            .iter()
            .any(|b: &QuotaBucket| b.balance.is_some_and(|m| m.currency == currency))
        {
            return Err(QuotaError::InvalidResponse);
        }
        let mut b = bucket(QuotaKind::Balance);
        b.balance = Some(Money {
            amount: amount.max(0.0),
            currency,
        });
        buckets.push(b);
    }
    summary(buckets)
}

/// Moonshot's balance: `available_balance` (cash and vouchers), in the platform's currency.
/// A negative or zero balance can't run requests, so it counts as none left.
fn parse_moonshot(value: &Value, currency: Currency) -> Result<QuotaSummary, QuotaError> {
    if value
        .get("code")
        .is_some_and(|code| code.as_i64() != Some(0))
    {
        return Err(QuotaError::InvalidResponse);
    }
    let amount = value["data"]["available_balance"]
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or(QuotaError::InvalidResponse)?;
    let mut b = bucket(QuotaKind::Balance);
    b.balance = Some(Money {
        amount: amount.max(0.0),
        currency,
    });
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
