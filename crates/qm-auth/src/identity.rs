//! Who a login signed in as: the identity reconnecting an account compares, so a login for the
//! account you already have replaces its credential in place, and a login for a different account
//! of the same provider is a new account.
//!
//! The identity is read locally from what the login returned, never from the network:
//!
//! - **ChatGPT**: the credential's account ID (the Codex login), else the access token's
//!   `chatgpt_account_id` claim, else its `sub` claim.
//! - **xAI, Kimi**: the access token's `sub` claim, when the token is a JWT.
//! - **Copilot**: unknown (the GitHub token is opaque; knowing the user would take a request).
//! - **API keys**: unknown (a key names no one).
//!
//! An identity is opaque: it is only compared, never shown or logged. Tokens are decoded here and
//! nothing of them leaves this module except the identity.

use crate::Credential;
use crate::chatgpt::account_id_from_jwt;
use base64::Engine as _;
use qm_types::domain::Provider;

/// Who `credential` (a login of `provider`) signed in as, when it can be known.
pub fn identity(provider: Provider, credential: &Credential) -> Option<String> {
    let Credential::OAuth {
        access_token,
        account_id,
        ..
    } = credential
    else {
        return None;
    };
    let token = access_token.expose();
    match provider {
        Provider::ChatGpt => account_id
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| account_id_from_jwt(token))
            .map(|a| format!("account:{a}"))
            .or_else(|| subject(token).map(|s| format!("sub:{s}"))),
        Provider::Xai | Provider::Kimi => subject(token).map(|s| format!("sub:{s}")),
        _ => None,
    }
}

/// Whether two identities say these are different people or accounts: only when both are known
/// and differ. An unknown identity never conflicts.
pub fn conflicts(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a != b)
}

/// The `sub` claim of a JWT's payload, if the token is a JWT with one. Decoded locally; never
/// logged.
fn subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("sub")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
