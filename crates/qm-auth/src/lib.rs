//! Credentials for non-Claude accounts, and the login flows that get them.
//!
//! Secrets live in one file only you can read: `~/.quartermaster/auth.json`, mode 0600, written
//! atomically (a temporary file, then a rename), with refreshed tokens saved in place. They are
//! never logged and never passed into an agent's environment. A Claude subscription is not here at
//! all: `claude` keeps its own login, which the app never reads, copies or stores.
//!
//! The contract every login and the provider engine use: a [`CredentialStore`] keyed by
//! [`CredentialRef`] (the key `accounts.credential_ref` holds, never the secret), holding
//! [`Credential`]s whose secret parts are [`Secret`]s. A `Secret` never shows in `Debug` output,
//! has no `Display`, and is deliberately **not** `Serialize`: neither is `Credential`, so no
//! event, log line, tool result or database row can carry one by accident. The file store maps
//! credentials to and from its own on-disk format with [`Secret::expose`] and [`Secret::new`].
//! It lives in `file.rs`, and each login flow in its own module.
//!
//! - [`login`]: one entry point for every login the Settings screen offers ([`login::Logins`]),
//!   each showing a browser address or a device code and finishing with a [`Credential`].
//! - [`tokens`]: keeps stored logins fresh ([`tokens::TokenKeeper`]): refreshes before expiry, one
//!   refresh at a time per credential, saves the new tokens in place at once, and records a login
//!   that stopped working in [`health::LoginHealth`] so routing skips only that account.
//!   Reconnecting an account saves the new login over the old one under the same key
//!   ([`TokenKeeper::save_in_place`]), which clears its problem.
//! - [`identity`]: who a login signed in as, so reconnecting tells the same account from a
//!   different one.
//! - Per provider: [`chatgpt`] (Sign in with ChatGPT, and the Codex login as its fallback),
//!   [`xai`], [`copilot`] and [`kimi`] (device code, [`device_code`]), [`openrouter`] (browser),
//!   and [`api_key`] (key checks for Anthropic, OpenAI, Gemini, xAI, OpenRouter and custom
//!   endpoints).
//!
//! Nothing in this crate starts a process, and no credential is ever put into an
//! environment.

pub mod api_key;
pub mod callback;
pub mod cancel;
pub mod chatgpt;
pub mod copilot;
pub mod device_code;
pub mod file;
pub mod health;
pub mod http;
pub mod identity;
pub mod kimi;
pub mod login;
pub mod openrouter;
pub mod pkce;
pub mod tokens;
pub mod xai;

pub use cancel::Cancel;
pub use file::FileCredentialStore;
pub use health::LoginHealth;
pub use login::{LoginEndpoints, LoginPrompt, LoginSession, Logins};
pub use tokens::TokenKeeper;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A secret value. `Debug` never shows it, there is no `Display`, and it is not `Serialize`:
/// reading it takes an explicit [`Secret::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself: for an HTTP header or the credential file, never for a log line.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The key of an account's credentials in the store, e.g. `chatgpt:work`. Stored in
/// `accounts.credential_ref`; not a secret.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CredentialRef(String);

impl CredentialRef {
    pub fn new(key: impl Into<String>) -> Result<Self, AuthError> {
        let key = key.into();
        if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(AuthError::InvalidRef(key));
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CredentialRef {
    type Error = AuthError;

    fn try_from(key: String) -> Result<Self, AuthError> {
        Self::new(key)
    }
}

impl From<CredentialRef> for String {
    fn from(r: CredentialRef) -> Self {
        r.0
    }
}

/// One account's credentials. Not `Serialize`, on purpose (see the crate docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// A pay-per-token API key (Anthropic, OpenAI, Gemini, xAI, OpenRouter, a custom endpoint).
    /// OpenRouter's browser login also ends with one: it issues a permanent key, not tokens.
    ApiKey { key: Secret },
    /// A subscription login's tokens (ChatGPT, Copilot, xAI, Kimi), refreshed automatically.
    /// For Copilot the refresh token is the GitHub token and the access token the Copilot token.
    OAuth {
        access_token: Secret,
        refresh_token: Option<Secret>,
        expires_at: Option<Timestamp>,
        /// The provider's account identifier, where a request needs it.
        account_id: Option<String>,
        /// The OAuth client ID the login was issued, where the provider issues one per login
        /// (Sign in with ChatGPT registers a new client every time); every refresh must send it.
        /// The credential file defaults it to `None`, so files written before it load unchanged.
        client_id: Option<String>,
    },
}

/// Where credentials are kept. Implementations write atomically and never log a secret.
pub trait CredentialStore: Send + Sync + fmt::Debug {
    fn get(&self, key: &CredentialRef) -> Result<Option<Credential>, AuthError>;
    fn put(&self, key: &CredentialRef, credential: Credential) -> Result<(), AuthError>;
    fn remove(&self, key: &CredentialRef) -> Result<(), AuthError>;
}

/// What can go wrong with credentials. Messages never carry a secret.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("{0:?} is not a valid credential key")]
    InvalidRef(String),
    #[error("the credential file can't be read or written: {0}")]
    Io(#[from] std::io::Error),
    #[error("the credential file is damaged: {0}")]
    Corrupt(String),
    #[error("the credential file's permissions are too open ({mode:o}); it must be 0600")]
    Permissions { mode: u32 },
    #[error("login failed: {0}")]
    Login(String),
    /// The provider refused the stored login or key (revoked, signed out elsewhere, wrong key):
    /// only connecting the account again fixes it.
    #[error("{0}; connect the account again in Settings ▸ Accounts")]
    Rejected(String),
    /// The device code or the wait for the browser ran out before the login finished.
    #[error("{0}")]
    Expired(String),
    /// You cancelled the login.
    #[error("the login was cancelled")]
    Cancelled,
}

impl AuthError {
    /// Whether this is a problem with the login itself (the account is unusable until fixed), as
    /// opposed to the credential file or a cancel.
    pub fn is_login_problem(&self) -> bool {
        matches!(self, Self::Login(_) | Self::Rejected(_) | Self::Expired(_))
    }

    /// Whether only connecting the account again fixes it.
    pub fn needs_reconnect(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }
}

/// `now + lifetime − skew` (never before now): when a token should count as expired.
pub(crate) fn expires_after(lifetime_secs: f64, skew: std::time::Duration) -> Timestamp {
    let secs = (lifetime_secs - skew.as_secs_f64()).max(0.0);
    Timestamp::now()
        .checked_add(SignedDuration::from_secs_f64(secs))
        .unwrap_or(Timestamp::MAX)
}

/// A subscription login's refresh token, or why there is none.
pub(crate) fn refresh_token_of<'a>(
    credential: &'a Credential,
    provider: &str,
) -> Result<&'a Secret, AuthError> {
    match credential {
        Credential::ApiKey { .. } => Err(AuthError::Login(format!(
            "the stored {provider} credential is an API key; it has nothing to refresh"
        ))),
        Credential::OAuth { refresh_token, .. } => refresh_token.as_ref().ok_or_else(|| {
            AuthError::Rejected(format!("the stored {provider} login has no refresh token"))
        }),
    }
}
