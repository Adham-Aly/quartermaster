//! Display-safe quota snapshots for connected accounts.
//!
//! ChatGPT's wham, Copilot's internal-user, Kimi Code's usages ([`kimi`]) and Grok's gRPC-web
//! credits meter ([`grok`]) are private provider APIs and may change: an answer they no longer
//! recognise is refused, never guessed at. OpenRouter exposes a public key-budget API (not the
//! whole account's balance). Each subscription is checked with its own login: ChatGPT (Codex),
//! Kimi and Grok refresh and retry once on a 401; Copilot sends its stored GitHub token.
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
//! sends or refreshes its token; so is an xAI or Kimi API key, since the meters take only the
//! subscription login. An answer with nothing metered in it (Copilot's zero-entitlement
//! placeholder, an empty Kimi or Grok reading, or a Grok meter that refuses a login xAI's API
//! still accepts) is [`NoQuotaCheck::NoReading`], a note, not a problem. A rate-limited check
//! carries the provider's `Retry-After`. Inference rate-limit headers are not account quota APIs.
//! Only allow-listed numeric fields and parsed timestamps escape this module, never provider
//! strings, response bodies, identifiers, tokens, or underlying authentication errors.

use crate::chatgpt::ChatGptFlow;
use crate::{Credential, CredentialRef, Secret, TokenKeeper};
use jiff::Timestamp;
use qm_types::domain::Provider;
use serde_json::Value;
use std::{fmt, sync::Arc, time::Duration};

mod grok;
mod kimi;

pub use grok::GrokProduct;

/// Trusted destinations for quota requests. Override only for a trusted service or local tests:
/// these URLs receive credentials. HTTPS is required except on loopback; redirects are refused.
#[derive(Clone, Debug)]
pub struct QuotaEndpoints {
    pub chatgpt: String,
    pub copilot: String,
    pub openrouter: String,
    /// Kimi Code's plan usage ([`kimi`]).
    pub kimi: String,
    /// Grok's subscription credits meter ([`grok`]).
    pub grok: String,
    /// xAI's model list, asked only when the meter refuses a login, to tell a lapsed login from
    /// one the meter doesn't serve.
    pub xai_models: String,
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
            kimi: url.into(),
            grok: url.into(),
            xai_models: url.into(),
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
            kimi: "https://api.kimi.com/coding/v1/usages".into(),
            grok: "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig".into(),
            xai_models: "https://api.x.ai/v1/models".into(),
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
    /// `retry_after`: the provider's `Retry-After`, in whole seconds, when it sent one.
    #[error("Quota requests are rate limited; try again later.")]
    RateLimited { retry_after: Option<Duration> },
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
    /// The check ran, but the provider's answer had nothing metered in it for this login.
    NoReading,
}

/// Written to follow "Provider quota: ".
impl fmt::Display for NoQuotaCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Claude => "not offered; Claude reports its usage during turns",
            Self::SignInWithChatGpt => "not offered for Sign in with ChatGPT",
            Self::NoSource => "not offered by this provider",
            Self::NoReading => "none reported for this login",
        })
    }
}

/// Why an account with this provider and base URL has no quota check, or `None` when
/// [`Quotas::fetch`] would ask one. Reads no credential and makes no request. A ChatGPT, xAI or
/// Kimi account gets `None` here; its stored login decides ([`NoQuotaCheck::SignInWithChatGpt`],
/// or [`NoQuotaCheck::NoSource`] for an API key).
pub fn no_check_for(provider: Provider, base_url: Option<&str>) -> Option<NoQuotaCheck> {
    match provider {
        Provider::Claude => Some(NoQuotaCheck::Claude),
        Provider::ChatGpt
        | Provider::Copilot
        | Provider::OpenRouter
        | Provider::Xai
        | Provider::Kimi => None,
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
    /// A monthly allowance with no stated length (Kimi Code's).
    Monthly,
    /// Grok's subscription credits, shared by every product.
    Credits,
    /// One Grok product's share of the credits; shown, never ranked.
    Product(GrokProduct),
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
            Self::Monthly => "Monthly",
            Self::Credits => "Credits",
            Self::Product(product) => product.name(),
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
    /// A Sign in with ChatGPT login, or an xAI or Kimi API key, returns [`QuotaError::NoCheck`]
    /// after reading the stored credential, never refreshing or sending it. A Codex, Kimi or Grok
    /// login's 401 retries once via `TokenKeeper::fresh`; Copilot uses the stored GitHub
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
            (Provider::Kimi, _) => &self.endpoints.kimi,
            (Provider::Xai, _) => &self.endpoints.grok,
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
        // Kimi's and Grok's meters take only the subscription login, never an API key.
        if matches!(provider, Provider::Kimi | Provider::Xai)
            && matches!(stored()?, Credential::ApiKey { .. })
        {
            return Err(QuotaError::NoCheck(NoQuotaCheck::NoSource));
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
                (Credential::OAuth { access_token, .. }, Provider::Kimi | Provider::Xai) => {
                    (access_token, None)
                }
                (Credential::OAuth { refresh_token, .. }, Provider::Copilot) => {
                    (refresh_token.as_ref().ok_or(QuotaError::Credentials)?, None)
                }
                _ => return Err(QuotaError::NoCheck(NoQuotaCheck::NoSource)),
            };
            let response = self.request(provider, endpoint, token, account_id).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && matches!(provider, Provider::ChatGpt | Provider::Kimi | Provider::Xai)
                && attempt == 0
            {
                credential = self
                    .tokens
                    .fresh(provider, key, Some(token))
                    .await
                    .map_err(|_| QuotaError::Credentials)?;
                continue;
            }
            return self.answer(provider, balance, response, token).await;
        }
        Err(QuotaError::Refused)
    }

    /// Reads a quota check's answer, once any 401 retry is behind it.
    async fn answer(
        &self,
        provider: Provider,
        balance: Option<BalanceSource>,
        response: reqwest::Response,
        token: &Secret,
    ) -> Result<QuotaSummary, QuotaError> {
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            // Only the provider and the numeric status: never a URL, token, body or ID.
            tracing::info!(%provider, status, "a quota check was refused or failed");
        }
        match status {
            200..=299 => {}
            401 | 403 if provider == Provider::Xai => return self.grok_refused(token).await,
            _ => return Err(failure(&response)),
        }
        if provider == Provider::Xai {
            let header = response
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = read_body(response).await?;
            return match grok::read(header.as_deref(), &body) {
                grok::Read::Summary(summary) => Ok(summary),
                grok::Read::Empty => Err(QuotaError::NoCheck(NoQuotaCheck::NoReading)),
                grok::Read::Refused => self.grok_refused(token).await,
                grok::Read::Failed(error) => Err(error),
            };
        }
        let value: Value = serde_json::from_slice(&read_body(response).await?)
            .map_err(|_| QuotaError::InvalidResponse)?;
        let parsed = match (provider, balance) {
            (_, Some(source)) => source.parse(&value).map(Some),
            (Provider::ChatGpt, _) => parse_chatgpt(&value, Timestamp::now()).map(Some),
            (Provider::Copilot, _) => parse_copilot(&value),
            (Provider::OpenRouter, _) => parse_openrouter(&value).map(Some),
            (Provider::Kimi, _) => kimi::parse(&value),
            _ => Err(QuotaError::NoCheck(NoQuotaCheck::NoSource)),
        };
        parsed?.ok_or(QuotaError::NoCheck(NoQuotaCheck::NoReading))
    }

    /// The Grok meter refused the login. That alone doesn't mean the login lapsed (the meter
    /// may not serve it), so xAI's API is asked once: if it takes the token there is just no
    /// reading; if it refuses it too, the login is refused.
    async fn grok_refused(&self, token: &Secret) -> Result<QuotaSummary, QuotaError> {
        validate_endpoint(&self.endpoints.xai_models)?;
        let response = self
            .http
            .get(&self.endpoints.xai_models)
            .bearer_auth(token.expose())
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| QuotaError::Unavailable)?;
        let status = response.status().as_u16();
        tracing::info!(
            status,
            "the Grok quota meter refused the login; xAI's API answered"
        );
        if (200..=299).contains(&status) {
            Err(QuotaError::NoCheck(NoQuotaCheck::NoReading))
        } else {
            Err(failure(&response))
        }
    }

    async fn request(
        &self,
        provider: Provider,
        endpoint: &str,
        token: &Secret,
        account_id: Option<&str>,
    ) -> Result<reqwest::Response, QuotaError> {
        let mut request = if provider == Provider::Xai {
            // gRPC-web: the content type, its marker and an empty request message, nothing more.
            self.http
                .post(endpoint)
                .header("accept", "*/*")
                .header("content-type", "application/grpc-web+proto")
                .header("x-grpc-web", "1")
                .body(grok::EMPTY_REQUEST.to_vec())
        } else {
            self.http.get(endpoint).header("accept", "application/json")
        }
        .bearer_auth(token.expose());
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

/// A non-2xx answer's fixed error.
fn failure(response: &reqwest::Response) -> QuotaError {
    match response.status().as_u16() {
        401 | 403 => QuotaError::Refused,
        429 => QuotaError::RateLimited {
            retry_after: response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        },
        _ => QuotaError::Unavailable,
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

/// The body, up to 1 MiB.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, QuotaError> {
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
    Ok(body)
}

fn percent(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
}

fn bucket(kind: QuotaKind) -> QuotaBucket {
    QuotaBucket::new(kind)
}

/// ChatGPT's windows. A window's length names it, not its slot (a Pro plan sends its weekly
/// window as primary). Percent used comes from `used_percent`, else `percent_remaining`; the
/// reset from `reset_at`, else `reset_after_seconds` from `now`. A slot with neither percent is
/// left out; one with a malformed percent refuses the answer.
fn parse_chatgpt(value: &Value, now: Timestamp) -> Result<QuotaSummary, QuotaError> {
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
        let (used, left) = (&window["used_percent"], &window["percent_remaining"]);
        let remaining = if !used.is_null() {
            100.0 - percent(used).ok_or(QuotaError::InvalidResponse)?
        } else if !left.is_null() {
            percent(left).ok_or(QuotaError::InvalidResponse)?
        } else {
            continue;
        };
        let mut b = bucket(kind);
        b.remaining_percent = Some(remaining);
        b.resets_at = window["reset_at"]
            .as_i64()
            .and_then(|s| Timestamp::from_second(s).ok())
            .or_else(|| {
                let after = window["reset_after_seconds"].as_i64()?;
                now.checked_add(jiff::SignedDuration::from_secs(after)).ok()
            });
        b.window_seconds = window["limit_window_seconds"]
            .as_u64()
            .or_else(|| window["window_seconds"].as_u64());
        buckets.push(b);
    }
    summary(buckets)
}

/// Copilot's allowances, `None` when nothing in them is metered. A zero-entitlement,
/// zero-remaining snapshot is GitHub's placeholder for a seat billed another way, not a used-up
/// allowance, so it is left out.
fn parse_copilot(value: &Value) -> Result<Option<QuotaSummary>, QuotaError> {
    // Only parse an absolute timestamp: a date-only reset must not invent a time or timezone.
    let reset = value["quota_reset_date"]
        .as_str()
        .and_then(|s| s.parse::<Timestamp>().ok());
    let reset_date = value["quota_reset_date"]
        .as_str()
        .filter(|s| s.len() == 10)
        .and_then(|s| s.parse::<jiff::civil::Date>().ok());
    let snapshots = value["quota_snapshots"]
        .as_object()
        .ok_or(QuotaError::InvalidResponse)?;
    let mut buckets = Vec::new();
    for (field, kind) in [
        ("chat", QuotaKind::Chat),
        ("completions", QuotaKind::Completions),
        ("premium_interactions", QuotaKind::PremiumRequests),
    ] {
        let Some(snapshot) = snapshots.get(field).filter(|s| !s.is_null()) else {
            continue;
        };
        let mut b = bucket(kind);
        b.unlimited = snapshot["unlimited"].as_bool().unwrap_or(false);
        b.remaining_percent = percent(&snapshot["percent_remaining"]);
        b.remaining = snapshot["remaining"].as_u64();
        b.entitlement = snapshot["entitlement"].as_u64();
        b.resets_at = reset;
        b.resets_on = reset_date;
        if !b.unlimited && b.entitlement == Some(0) && b.remaining == Some(0) {
            continue;
        }
        if !b.unlimited && b.remaining_percent.is_none() && b.remaining.is_none() {
            return Err(QuotaError::InvalidResponse);
        }
        buckets.push(b);
    }
    Ok((!buckets.is_empty()).then(|| QuotaSummary::new(buckets)))
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
