//! Which logins are working: each login is isolated, so one breaking marks only its own account
//! unusable, and routing skips that account until it works again.
//!
//! [`crate::TokenKeeper`] records here: a refresh refused by the provider, or a stored login that
//! can't be used, marks that credential's problem; a successful refresh or a new login clears it.
//! The core reads [`LoginHealth::problem`] when it builds routing's candidates and leaves out an
//! account that has one, with the problem as the "why" line. Problems are plain sentences that
//! never carry a secret. Nothing here is persisted: a restart tries every login again.

use crate::{AuthError, CredentialRef};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// The login problems known right now, per credential.
#[derive(Debug, Default)]
pub struct LoginHealth {
    problems: Mutex<HashMap<CredentialRef, String>>,
}

impl LoginHealth {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<CredentialRef, String>> {
        // Plain data; a panic elsewhere leaves nothing half-done.
        self.problems
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `error` against `key` if only connecting the account again fixes it (a revoked or
    /// refused login). A network error, a server error, a cancel or a file error says nothing
    /// lasting about the account, so it is not recorded: the next request simply tries again.
    pub fn record(&self, key: &CredentialRef, error: &AuthError) {
        if error.needs_reconnect() {
            self.mark(key, error.to_string());
        }
    }

    /// Marks `key` unusable with a plain-sentence reason (for example, no login is stored).
    pub fn mark(&self, key: &CredentialRef, problem: impl Into<String>) {
        self.lock().insert(key.clone(), problem.into());
    }

    /// Marks `key` as working again (a refresh succeeded, or you connected it again).
    pub fn clear(&self, key: &CredentialRef) {
        self.lock().remove(key);
    }

    /// What is wrong with `key`'s login, if anything.
    pub fn problem(&self, key: &CredentialRef) -> Option<String> {
        self.lock().get(key).cloned()
    }

    /// Whether routing may use the account whose credential is `key`.
    pub fn is_usable(&self, key: &CredentialRef) -> bool {
        !self.lock().contains_key(key)
    }

    /// Every credential with a problem, for the Settings screen and the setup check.
    pub fn problems(&self) -> Vec<(CredentialRef, String)> {
        let mut all: Vec<_> = self
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        all.sort();
        all
    }
}
