//! Provider quotas: the lines Settings ▸ Accounts shows live only in memory; each successful
//! check's numbers and reset times are also stored on the account (`accounts.quota`, through
//! `crate::quota_pools`) for quota-aware routing. The new pools are worked out on the database
//! thread from the reading stored there, so a balance keeps its last top-up between checks.
//! An account with no quota check (Claude, keys without a quota API, a Sign in with ChatGPT
//! login) gets a neutral note instead, and makes no request. A check whose answer had nothing
//! metered in it gets a note too, and clears the account's stored pools. A rate-limited check
//! waits for the provider's `Retry-After` (else ten minutes) before asking again.
use super::{AccountRow, Inner, accounts};
use qm_auth::{
    CredentialRef,
    quota::{NoQuotaCheck, QuotaError, QuotaSummary, Quotas},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const PERIOD: Duration = Duration::from_secs(300);
const OPEN_COOLDOWN: Duration = Duration::from_secs(30);
/// How long a rate-limited check waits when the provider doesn't say.
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(600);
/// The bounds on a provider's `Retry-After`.
const RETRY_AFTER_BOUNDS: (Duration, Duration) =
    (Duration::from_secs(60), Duration::from_secs(3_600));

#[derive(Default)]
pub(super) struct CachedQuota {
    pub lines: Vec<String>,
    pub problem: Option<String>,
    /// Why the account has no quota check; never a problem.
    pub note: Option<String>,
    pub refreshing: bool,
    pub checked_at: Option<String>,
    attempted: Option<Instant>,
    /// After a rate limit: no check before this, even when Settings opens.
    not_before: Option<Instant>,
    /// Distinguishes reconnect generations even when the account and credential key stay the same.
    generation: Arc<()>,
}

impl CachedQuota {
    pub(super) fn fill(&self, account: &mut qm_types::api::SettingsAccount) {
        account.quota_lines.clone_from(&self.lines);
        account.quota_problem.clone_from(&self.problem);
        account.quota_refreshing = self.refreshing;
        account.quota_checked_at.clone_from(&self.checked_at);
        account.quota_note.clone_from(&self.note);
    }

    /// The account has no quota check: say why, and drop anything an earlier check left.
    fn no_check(&mut self, why: NoQuotaCheck) {
        self.note = Some(why.to_string());
        self.problem = None;
        self.lines.clear();
        self.checked_at = None;
    }
}

pub(super) fn start(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let clock = Arc::clone(&inner.clock);
    let quotas: Option<Arc<Quotas>> = None;
    tokio::spawn(async move {
        loop {
            clock.sleep(PERIOD).await;
            let Some(inner) = weak.upgrade() else { break };
            refresh_with(&inner, false, quotas.clone()).await;
        }
    });
}

/// Starts bounded background requests, never waiting for provider HTTP on the UI path.
/// Snapshot fills also discover newly connected accounts immediately.
pub(super) async fn refresh(inner: &Arc<Inner>, opened: bool) {
    refresh_with(inner, opened, None).await;
}

async fn refresh_with(inner: &Arc<Inner>, opened: bool, quotas: Option<Arc<Quotas>>) {
    let Ok(accounts) = inner.db.call(|conn| accounts::list_active(conn)).await else {
        return;
    };
    let now = inner.clock.instant();
    let interval = if opened { OPEN_COOLDOWN } else { PERIOD };
    let mut requests = Vec::new();
    {
        let mut state = inner.lock();
        state
            .quotas
            .retain(|id, _| accounts.iter().any(|a| a.id == *id));
        for account in accounts {
            let cached = state.quotas.entry(account.id).or_default();
            // Decided from the provider alone, with no request and no credential read.
            if let Some(why) =
                qm_auth::quota::no_check_for(account.provider, account.base_url.as_deref())
            {
                cached.no_check(why);
                continue;
            }
            if cached.refreshing
                || !due(cached.attempted, now, interval)
                || cached.not_before.is_some_and(|at| now < at)
            {
                continue;
            }
            cached.refreshing = true;
            cached.attempted = Some(now);
            requests.push((account, Arc::clone(&cached.generation)));
        }
    }
    if requests.is_empty() {
        return;
    }
    let quotas = quotas.unwrap_or_else(|| {
        Arc::new(Quotas::with_endpoints(
            Arc::clone(&inner.tokens),
            inner.quota_endpoints.clone(),
        ))
    });
    for (account, generation) in requests {
        let inner = Arc::clone(inner);
        let quotas = Arc::clone(&quotas);
        tokio::spawn(async move { check(&inner, &quotas, &account, &generation).await });
    }
}

/// Runs one account's check and records what it found.
async fn check(inner: &Arc<Inner>, quotas: &Quotas, account: &AccountRow, generation: &Arc<()>) {
    let result = match account
        .credential_ref
        .as_deref()
        .and_then(|key| CredentialRef::new(key).ok())
    {
        Some(key) => {
            quotas
                .fetch(account.provider, &key, account.base_url.as_deref())
                .await
        }
        None => Err(QuotaError::NoCheck(NoQuotaCheck::NoSource)),
    };
    let mut pools = None;
    {
        let mut state = inner.lock();
        // A disconnected account's in-flight request must not recreate its cache.
        if let Some(cached) = state.quotas.get_mut(&account.id)
            && Arc::ptr_eq(&cached.generation, generation)
        {
            cached.refreshing = false;
            cached.not_before = None;
            match result {
                Ok(summary) => {
                    cached.lines = summary.buckets.iter().map(ToString::to_string).collect();
                    cached.problem = None;
                    cached.note = None;
                    cached.checked_at = Some(inner.clock.now().to_string());
                    pools = Some(summary);
                }
                Err(QuotaError::NoCheck(why)) => {
                    cached.no_check(why);
                    // Nothing metered: an earlier reading's pools no longer hold.
                    if why == NoQuotaCheck::NoReading {
                        pools = Some(QuotaSummary::new(Vec::new()));
                    }
                }
                Err(error) => {
                    if let QuotaError::RateLimited { retry_after } = error {
                        cached.not_before =
                            Some(inner.clock.instant() + rate_limit_wait(retry_after));
                    }
                    cached.problem = Some(error.to_string());
                }
            }
        }
    }
    if let Some(summary) = pools {
        let (id, at) = (account.id, inner.clock.now());
        if let Err(e) = inner
            .db
            .call(move |conn| {
                let before = accounts::load_account(conn, id)?.and_then(|a| a.quota);
                let stored = crate::quota_pools::from_summary(&summary, before.as_deref());
                match serde_json::to_string(&stored) {
                    Ok(json) => accounts::record_quota(conn, id, &json, at),
                    Err(_) => Ok(()),
                }
            })
            .await
        {
            tracing::warn!(error = %e, "couldn't store the account's quota reading");
        }
    }
    inner.changed();
}

/// How long to wait after a rate limit: the provider's `Retry-After`, kept within bounds.
fn rate_limit_wait(retry_after: Option<Duration>) -> Duration {
    let (low, high) = RETRY_AFTER_BOUNDS;
    retry_after.map_or(RATE_LIMIT_WAIT, |d| d.clamp(low, high))
}

fn due(attempted: Option<Instant>, now: Instant, interval: Duration) -> bool {
    attempted.is_none_or(|at| now.saturating_duration_since(at) >= interval)
}
