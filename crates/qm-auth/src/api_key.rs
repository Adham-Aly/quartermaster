//! Pay-per-token API keys: Anthropic, OpenAI, Gemini, xAI, OpenRouter, and any OpenAI-compatible
//! endpoint (a base URL, a key and a model list).
//!
//! Before a key is saved it is checked with one cheap, free request that needs it: listing the
//! provider's models (OpenRouter's model list is public, so its key-info endpoint instead). A 401
//! or 403 means the key is wrong ([`AuthError::Rejected`]); a network error or a server error
//! means it couldn't be checked ([`AuthError::Login`]), and nothing is saved either way. A custom
//! endpoint that doesn't list models (404 or 405) is accepted unchecked, with your model list.
//!
//! The key is sent only in its provider's auth header and never appears in an error or log line.

use crate::http::{self, Reply};
use crate::{AuthError, Credential, Secret};
use qm_types::domain::Provider;

/// Where each provider's key is checked. [`Default`] is the providers' own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeyEndpoints {
    pub anthropic: String,
    pub openai: String,
    pub gemini: String,
    pub xai: String,
    pub openrouter: String,
}

impl Default for ApiKeyEndpoints {
    fn default() -> Self {
        Self {
            anthropic: "https://api.anthropic.com/v1/models".into(),
            openai: "https://api.openai.com/v1/models".into(),
            gemini: "https://generativelanguage.googleapis.com/v1beta/models".into(),
            xai: "https://api.x.ai/v1/models".into(),
            openrouter: "https://openrouter.ai/api/v1/key".into(),
        }
    }
}

/// The providers whose keys can be added (a custom endpoint goes through [`check_custom`]).
pub const KEY_PROVIDERS: &[Provider] = &[
    Provider::Anthropic,
    Provider::OpenAi,
    Provider::Gemini,
    Provider::Xai,
    Provider::OpenRouter,
];

/// What a key check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyCheck {
    /// The model IDs the provider listed for this key (empty when it lists none, or for
    /// OpenRouter, whose list doesn't depend on the key).
    pub models: Vec<String>,
    /// Whether the key was actually checked (false only for a custom endpoint that lists no
    /// models).
    pub verified: bool,
}

/// A key as typed or pasted, tidied: surrounding whitespace removed. Refuses an empty key or one
/// with whitespace or control characters inside (a paste that caught more than the key).
pub fn normalise(key: &str) -> Result<Secret, AuthError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(AuthError::Login("the key is empty".into()));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AuthError::Login(
            "the key has spaces or line breaks in it; paste only the key".into(),
        ));
    }
    Ok(Secret::new(key))
}

/// Checks a provider's key with one request; on success the caller stores
/// `Credential::ApiKey { key }`.
pub async fn check(
    http: &reqwest::Client,
    endpoints: &ApiKeyEndpoints,
    provider: Provider,
    key: &Secret,
) -> Result<KeyCheck, AuthError> {
    let name = display_name(provider);
    let request = match provider {
        Provider::Anthropic => http
            .get(&endpoints.anthropic)
            .header("x-api-key", key.expose())
            .header("anthropic-version", "2023-06-01"),
        Provider::Gemini => http
            .get(&endpoints.gemini)
            .header("x-goog-api-key", key.expose()),
        Provider::OpenAi => http.get(&endpoints.openai).bearer_auth(key.expose()),
        Provider::Xai => http.get(&endpoints.xai).bearer_auth(key.expose()),
        Provider::OpenRouter => http.get(&endpoints.openrouter).bearer_auth(key.expose()),
        other => {
            return Err(AuthError::Login(format!(
                "{other} accounts aren't connected with an API key"
            )));
        }
    };
    let reply = http::send(
        request.header("accept", "application/json"),
        name,
        "key check",
    )
    .await?;
    if !reply.ok() {
        return Err(refused(&reply, name));
    }
    let models = if provider == Provider::OpenRouter {
        Vec::new()
    } else {
        model_ids(&reply, provider == Provider::Gemini)
    };
    Ok(KeyCheck {
        models,
        verified: true,
    })
}

/// An OpenAI-compatible endpoint, as you entered it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomEndpoint {
    /// The API base, e.g. `https://llm.example.com/v1` (no trailing slash).
    pub base_url: String,
    /// The models you listed.
    pub models: Vec<String>,
}

/// Tidies and checks a custom endpoint's base URL: `https`, or `http` on this Mac or the local
/// network's `.local` names; no query, fragment or credentials in it; no trailing slash.
pub fn normalise_base_url(raw: &str) -> Result<String, AuthError> {
    let url = url::Url::parse(raw.trim())
        .map_err(|_| AuthError::Login("the base URL isn't a valid URL".into()))?;
    let host = url.host_str().unwrap_or_default();
    let local = matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        || host
            .rsplit_once('.')
            .is_some_and(|(_, tld)| tld.eq_ignore_ascii_case("local"));
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        _ => {
            return Err(AuthError::Login(
                "the base URL must start with https:// (http:// only for this Mac)".into(),
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AuthError::Login(
            "put the key in the key field, not in the base URL".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AuthError::Login(
            "the base URL can't have a query or a fragment".into(),
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Checks a custom endpoint's key by listing `<base>/models`. The key may be empty for an endpoint
/// that needs none (`None`).
pub async fn check_custom(
    http: &reqwest::Client,
    endpoint: &CustomEndpoint,
    key: Option<&Secret>,
) -> Result<KeyCheck, AuthError> {
    let base = normalise_base_url(&endpoint.base_url)?;
    let mut request = http
        .get(format!("{base}/models"))
        .header("accept", "application/json");
    if let Some(key) = key {
        request = request.bearer_auth(key.expose());
    }
    let reply = http::send(request, "the endpoint", "key check").await?;
    if matches!(reply.status, 404 | 405) {
        return Ok(KeyCheck {
            models: endpoint.models.clone(),
            verified: false,
        });
    }
    if !reply.ok() {
        return Err(refused(&reply, "the endpoint"));
    }
    Ok(KeyCheck {
        models: model_ids(&reply, false),
        verified: true,
    })
}

/// The credential a checked key is stored as.
pub fn credential(key: Secret) -> Credential {
    Credential::ApiKey { key }
}

fn refused(reply: &Reply, name: &str) -> AuthError {
    match reply.status {
        401 | 403 => AuthError::Rejected(format!(
            "{name} didn't accept the key (HTTP {}); check it and try again",
            reply.status
        )),
        // Gemini answers a bad key with 400 `API_KEY_INVALID`.
        400 if reply
            .body
            .get("error")
            .and_then(|e| e.get("status").or_else(|| e.get("message")))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| s.contains("API_KEY") || s.contains("API key")) =>
        {
            AuthError::Rejected(format!(
                "{name} didn't accept the key (HTTP 400); check it and try again"
            ))
        }
        _ => reply.failure(name, "key check"),
    }
}

/// Model IDs from `{"data":[{"id":…}]}` (OpenAI style) or `{"models":[{"name":"models/…"}]}`
/// (Gemini).
fn model_ids(reply: &Reply, gemini: bool) -> Vec<String> {
    let (list, field) = if gemini {
        ("models", "name")
    } else {
        ("data", "id")
    };
    reply
        .body
        .get(list)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|m| m.get(field).and_then(serde_json::Value::as_str))
                .map(|id| id.strip_prefix("models/").unwrap_or(id).to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn display_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "Anthropic",
        Provider::OpenAi => "OpenAI",
        Provider::Gemini => "Gemini",
        Provider::Xai => "xAI",
        Provider::OpenRouter => "OpenRouter",
        _ => "the provider",
    }
}
