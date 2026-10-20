//! What the adapters (Anthropic Messages, Chat Completions, Gemini) share: the effort setting
//! read through the catalog, the catalog's compatibility flags, context-overflow detection, and
//! building classified errors.
//!
//! Pure functions, like the adapters themselves.

use super::{ClassifiedError, ErrorClass, ModelRequest};
use crate::catalog::detect::canonical_id;
use crate::catalog::{Catalog, CatalogModel};
use qm_engine::{ProviderError, ProviderErrorKind};
use qm_types::domain::Provider;
use qm_types::ids::ModelId;
use regex::RegexSet;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The provider value that means "thinking off" (some providers offer it). A catalog
/// `effort_values` entry may map a rung to it; each adapter turns it into its API's way of
/// disabling thinking.
pub(crate) const EFFORT_OFF: &str = "off";

/// What a request says about reasoning effort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EffortSetting {
    /// Send no effort setting: the model doesn't reason, takes no effort setting, or doesn't
    /// support this rung (an unsupported level is never sent).
    Omit,
    /// Turn thinking off (the catalog maps this rung to `off`).
    Off,
    /// Send this provider value (`minimal`, `low`, … as the catalog maps the rung).
    Value(String),
}

/// The catalog entry for a provider's model: by the id the account uses, then by its canonical
/// id. `None` when the catalog doesn't know it (or failed to load, which startup reports).
pub(crate) fn catalog_model(provider: Provider, id: &ModelId) -> Option<&'static CatalogModel> {
    let catalog = Catalog::embedded().ok()?;
    catalog
        .lookup(provider, id)
        .or_else(|| catalog.by_canonical(provider, &canonical_id(id)))
}

/// The effort setting for `req`, through the catalog.
///
/// A model the catalog knows decides by its supported rungs and their provider values. A model it
/// doesn't know falls back to the profile's rungs (the engine's model list, itself filled from
/// the catalog's cautious defaults), each sent as its own name.
pub(crate) fn effort_setting(req: &ModelRequest<'_>) -> EffortSetting {
    if !req.model.reasoning {
        return EffortSetting::Omit;
    }
    match catalog_model(req.provider, &req.model.id) {
        Some(entry) => match entry.effort_value(req.effort) {
            Some(EFFORT_OFF) => EffortSetting::Off,
            Some(value) => EffortSetting::Value(value.to_owned()),
            None => EffortSetting::Omit,
        },
        None if req.model.efforts.contains(&req.effort) => {
            EffortSetting::Value(req.effort.as_str().to_owned())
        }
        None => EffortSetting::Omit,
    }
}

/// The lowest provider value the catalog offers for this model other than `off` (Gemini's
/// thinking-level models can't turn thinking off, so "off" becomes the lowest level).
pub(crate) fn lowest_effort_value(provider: Provider, id: &ModelId) -> Option<String> {
    let entry = catalog_model(provider, id)?;
    entry
        .efforts
        .iter()
        .filter_map(|e| entry.effort_value(*e))
        .find(|v| *v != EFFORT_OFF)
        .map(str::to_owned)
}

/// The catalog's compatibility flags for a provider's model.
///
/// A model the catalog knows has its own (the provider's defaults with the model's corrections
/// on top). For one it doesn't know, the provider's defaults are what every one of its catalog
/// models agrees on; a provider with no catalog models (a custom endpoint) has none, so the
/// adapter's standard behaviour applies.
pub(crate) fn compat_flags(provider: Provider, id: &ModelId) -> BTreeMap<String, bool> {
    if let Some(entry) = catalog_model(provider, id) {
        return entry.compat.clone();
    }
    let Ok(catalog) = Catalog::embedded() else {
        return BTreeMap::new();
    };
    let mut models = catalog.models(provider).iter();
    let Some(first) = models.next() else {
        return BTreeMap::new();
    };
    let mut agreed = first.compat.clone();
    for model in models {
        agreed.retain(|k, v| model.compat.get(k) == Some(v));
    }
    agreed
}

/// Whether an error message says the context is too long for the model (Pi's
/// `utils/overflow.ts` patterns at the pin, for the providers we serve).
pub(crate) fn is_context_overflow(message: &str) -> bool {
    static PATTERNS: OnceLock<Option<(RegexSet, RegexSet)>> = OnceLock::new();
    let Some((overflow, not_overflow)) = PATTERNS.get_or_init(|| {
        let overflow = RegexSet::new([
            r"(?i)prompt (?:is )?too long",
            r"(?i)request_too_large",
            r"(?i)exceeds the context window",
            r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
            r"(?i)input token count.*exceeds the maximum",
            r"(?i)maximum prompt length is \d+",
            r"(?i)maximum context length is \d+ tokens",
            r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
            r"(?i)exceeds the limit of \d+",
            r"(?i)exceeded model token limit",
            r"(?i)context[_ ]length[_ ]exceeded",
            r"(?i)too many tokens",
            r"(?i)token limit exceeded",
        ])
        .ok()?;
        let not_overflow = RegexSet::new([r"(?i)rate limit", r"(?i)too many requests"]).ok()?;
        Some((overflow, not_overflow))
    }) else {
        return false;
    };
    overflow.is_match(message) && !not_overflow.is_match(message)
}

/// A provider error with its class.
pub(crate) fn classified(
    kind: ProviderErrorKind,
    status: Option<u16>,
    message: String,
    class: ErrorClass,
) -> ClassifiedError {
    ClassifiedError::new(
        ProviderError {
            kind,
            status,
            message,
        },
        class,
    )
}

/// `<code>: <message>` (cut to 300 characters), or whichever of them there is.
pub(crate) fn error_detail(code: Option<&str>, message: Option<&str>) -> String {
    match (code, message) {
        (Some(c), Some(m)) => format!("{c}: {}", super::responses::cut(m, 300)),
        (Some(c), None) => c.to_owned(),
        (None, Some(m)) => super::responses::cut(m, 300),
        (None, None) => "no details".to_owned(),
    }
}

/// Classifies a failed HTTP response by status once the provider-specific cases (account
/// limits, expired logins) are handled: overflow, then the retryable statuses, then fatal.
pub(crate) fn classify_status(status: u16, message: String, overflow: bool) -> ClassifiedError {
    let s = Some(status);
    if status == 401 {
        return classified(
            ProviderErrorKind::Auth,
            s,
            message,
            ErrorClass::Unauthorized,
        );
    }
    if overflow && matches!(status, 400 | 413) {
        return classified(
            ProviderErrorKind::ContextOverflow,
            s,
            message,
            ErrorClass::ContextOverflow,
        );
    }
    match status {
        408 => classified(
            ProviderErrorKind::Network,
            s,
            message,
            ErrorClass::Retryable,
        ),
        409 => classified(ProviderErrorKind::Other, s, message, ErrorClass::Retryable),
        429 => classified(
            ProviderErrorKind::RateLimited,
            s,
            message,
            ErrorClass::Retryable,
        ),
        503 | 529 => classified(
            ProviderErrorKind::Overloaded,
            s,
            message,
            ErrorClass::Retryable,
        ),
        500.. => classified(ProviderErrorKind::Server, s, message, ErrorClass::Retryable),
        402 | 403 => classified(ProviderErrorKind::Auth, s, message, ErrorClass::Fatal),
        _ => classified(
            ProviderErrorKind::InvalidRequest,
            s,
            message,
            ErrorClass::Fatal,
        ),
    }
}

/// Applies `x-should-retry` (sent by OpenAI-style and Anthropic APIs) to a classification that
/// isn't an account limit, an expired login or an overflow.
pub(crate) fn honour_should_retry(
    mut error: ClassifiedError,
    header: Option<&str>,
) -> ClassifiedError {
    if !matches!(
        error.class,
        ErrorClass::AccountLimit { .. } | ErrorClass::Unauthorized | ErrorClass::ContextOverflow
    ) {
        match header {
            Some("true") => error.class = ErrorClass::Retryable,
            Some("false") => error.class = ErrorClass::Fatal,
            _ => {}
        }
    }
    error
}

/// A stream that ended without its terminal event: worth another attempt.
pub(crate) fn no_terminal_event(what: &str) -> ClassifiedError {
    classified(
        ProviderErrorKind::Network,
        None,
        format!("the {what} stream ended before it finished"),
        ErrorClass::Retryable,
    )
}

/// An event that isn't JSON: worth another attempt.
pub(crate) fn not_json(error: &serde_json::Error) -> ClassifiedError {
    classified(
        ProviderErrorKind::Other,
        None,
        format!("the stream sent an event that isn't JSON: {error}"),
        ErrorClass::Retryable,
    )
}

/// A tool call's arguments as an object for APIs that take one (Anthropic `input`, Gemini
/// `args`): arguments that weren't valid JSON are sent as `{}` (the loop answered that call with
/// an error result).
pub(crate) fn args_object(args: &serde_json::Value) -> serde_json::Value {
    if args.is_object() {
        args.clone()
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

/// A tool call's arguments as the JSON text APIs that take a string want (Chat Completions):
/// arguments kept raw (invalid JSON) are sent as they came.
pub(crate) fn args_text(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}
