//! Keeping stored logins fresh.
//!
//! [`TokenKeeper::fresh`] returns an account's credential, refreshed first when it expires within
//! [`EXPIRY_MARGIN`] or when the caller says its token was just refused (a 401). Refreshes are one
//! at a time per credential key: refresh tokens rotate, so two refreshes racing would leave one
//! side with a dead token. The waiting side re-reads the store and uses the other's result. The new
//! tokens are saved in place before anything else can fail, through the store's atomic write.
//!
//! A refresh the provider refuses (a revoked or signed-out login) is returned as
//! [`AuthError::Rejected`] and recorded in [`LoginHealth`], so routing skips only that account; a
//! network or server error is returned and not recorded, and the next request tries again.
//! Nothing here panics on a provider's answer, and no token is logged.

use crate::chatgpt::{self, EXPIRY_MARGIN};
use crate::health::LoginHealth;
use crate::login::LoginEndpoints;
use crate::{AuthError, Credential, CredentialRef, CredentialStore, Secret};
use jiff::{SignedDuration, Timestamp};
use qm_types::domain::Provider;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Refreshes stored logins and records the ones that stopped working.
pub struct TokenKeeper {
    store: Arc<dyn CredentialStore>,
    endpoints: LoginEndpoints,
    health: Arc<LoginHealth>,
    http: reqwest::Client,
    locks: Mutex<HashMap<CredentialRef, Arc<tokio::sync::Mutex<()>>>>,
}

impl fmt::Debug for TokenKeeper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenKeeper")
            .field("store", &self.store)
            .field("health", &self.health)
            .finish_non_exhaustive()
    }
}

impl TokenKeeper {
    pub fn new(
        store: Arc<dyn CredentialStore>,
        endpoints: LoginEndpoints,
        health: Arc<LoginHealth>,
    ) -> Self {
        Self {
            store,
            endpoints,
            health,
            http: crate::http::client(),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn health(&self) -> &Arc<LoginHealth> {
        &self.health
    }

    pub fn store(&self) -> &Arc<dyn CredentialStore> {
        &self.store
    }

    /// The credential for `key` (an account of `provider`), refreshed first when it is about to
    /// expire, or when `refused` is the token a request just had refused and the store still
    /// holds it.
    pub async fn fresh(
        &self,
        provider: Provider,
        key: &CredentialRef,
        refused: Option<&Secret>,
    ) -> Result<Credential, AuthError> {
        let credential = self.read(key)?;
        if !needs_refresh(&credential, refused, Timestamp::now()) {
            return Ok(credential);
        }
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        // Another task may have refreshed while this one waited.
        let current = self.read(key)?;
        if !needs_refresh(&current, refused, Timestamp::now()) {
            return Ok(current);
        }
        match refresh(&self.http, &self.endpoints, provider, &current).await {
            Ok(refreshed) => {
                // Refresh tokens rotate: save the new one before anything else can fail.
                self.store.put(key, refreshed.clone())?;
                self.health.clear(key);
                tracing::info!(credential = %key, %provider, "refreshed a login");
                Ok(refreshed)
            }
            Err(error) => {
                self.health.record(key, &error);
                tracing::warn!(credential = %key, %provider, reconnect = error.needs_reconnect(),
                    "a login refresh failed");
                Err(error)
            }
        }
    }

    /// Saves a new login's credential (from [`crate::LoginSession::finish`] or a checked key) and
    /// marks the account usable.
    pub fn save_new(&self, key: &CredentialRef, credential: Credential) -> Result<(), AuthError> {
        self.store.put(key, credential)?;
        self.health.clear(key);
        Ok(())
    }

    /// Saves a login that reconnects an existing account over its old credential, under the same
    /// key, and marks the account usable again. It waits for any refresh of that key in flight, so
    /// a refresh of the old login can't overwrite the new one or mark it broken after this clears
    /// it. The store's write is atomic: the old credential or the new one, never half of either.
    pub async fn save_in_place(
        &self,
        key: &CredentialRef,
        credential: Credential,
    ) -> Result<(), AuthError> {
        let lock = self.key_lock(key);
        let _guard = lock.lock().await;
        self.store.put(key, credential)?;
        self.health.clear(key);
        tracing::info!(credential = %key, "reconnected a login in place");
        Ok(())
    }

    /// The credential stored under `key`, as it is (no refresh, nothing recorded); `None` if
    /// there is none.
    pub fn stored(&self, key: &CredentialRef) -> Result<Option<Credential>, AuthError> {
        self.store.get(key)
    }

    /// Forgets an account's credential (Disconnect).
    pub fn forget(&self, key: &CredentialRef) -> Result<(), AuthError> {
        self.store.remove(key)?;
        self.health.clear(key);
        Ok(())
    }

    /// The lock that keeps one write at a time per credential key.
    fn key_lock(&self, key: &CredentialRef) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(key.clone()).or_default())
    }

    fn read(&self, key: &CredentialRef) -> Result<Credential, AuthError> {
        if let Some(credential) = self.store.get(key)? {
            return Ok(credential);
        }
        let error = AuthError::Rejected(format!("no login is stored for {key}"));
        self.health.record(key, &error);
        Err(error)
    }
}

/// Refreshes a credential of `provider`. API keys (and OpenRouter's key) come back unchanged.
/// The caller saves the result at once.
pub async fn refresh(
    http: &reqwest::Client,
    endpoints: &LoginEndpoints,
    provider: Provider,
    credential: &Credential,
) -> Result<Credential, AuthError> {
    if matches!(credential, Credential::ApiKey { .. }) {
        return Ok(credential.clone());
    }
    match provider {
        Provider::ChatGpt => chatgpt::refresh_with(http, &endpoints.chatgpt, credential).await,
        Provider::Xai => crate::xai::refresh(http, &endpoints.xai, credential).await,
        Provider::Copilot => crate::copilot::refresh(http, &endpoints.copilot, credential).await,
        Provider::Kimi => crate::kimi::refresh(http, &endpoints.kimi, credential).await,
        other => Err(AuthError::Rejected(format!(
            "a {other} account can't hold a subscription login"
        ))),
    }
}

/// Whether a credential must be refreshed before use.
pub fn needs_refresh(credential: &Credential, refused: Option<&Secret>, now: Timestamp) -> bool {
    match credential {
        Credential::ApiKey { .. } => false,
        Credential::OAuth {
            access_token,
            expires_at,
            ..
        } => {
            if let Some(refused) = refused {
                // Refresh only if the stored token is still the refused one.
                return access_token == refused;
            }
            expires_at.is_some_and(|at| expiring(at, now))
        }
    }
}

fn expiring(expires_at: Timestamp, now: Timestamp) -> bool {
    let margin = SignedDuration::try_from(EXPIRY_MARGIN).unwrap_or_default();
    now.checked_add(margin)
        .map_or(true, |soon| soon >= expires_at)
}
