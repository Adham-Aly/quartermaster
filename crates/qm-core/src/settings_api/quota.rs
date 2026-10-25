//! Provider quotas: the lines Settings ▸ Accounts shows live only in memory; each successful
//! check's numbers and reset times are also stored on the account (`accounts.quota`, through
//! `crate::quota_pools`) for quota-aware routing. The new pools are worked out on the database
//! thread from the reading stored there, so a balance keeps its last top-up between checks.
//! An account with no quota check (Claude, keys without a quota API, a Sign in with ChatGPT
//! login) gets a neutral note instead, and makes no request.
use super::{Inner, accounts};
use qm_auth::{
    CredentialRef,
    quota::{NoQuotaCheck, QuotaError, Quotas},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const PERIOD: Duration = Duration::from_secs(300);
const OPEN_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct CachedQuota {
    pub lines: Vec<String>,
    pub problem: Option<String>,
    /// Why the account has no quota check; never a problem.
    pub note: Option<String>,
    pub refreshing: bool,
    pub checked_at: Option<String>,
    attempted: Option<Instant>,
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

// Task-local injection keeps parallel tests isolated and never changes production endpoints.
#[cfg(test)]
tokio::task_local! {
    static TEST_QUOTAS: Arc<Quotas>;
}

fn injected_client() -> Option<Arc<Quotas>> {
    #[cfg(test)]
    return TEST_QUOTAS.try_with(Arc::clone).ok();
    #[cfg(not(test))]
    None
}

pub(super) fn start(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let clock = Arc::clone(&inner.clock);
    let quotas = injected_client();
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
    refresh_with(inner, opened, injected_client()).await;
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
            if cached.refreshing || !due(cached.attempted, now, interval) {
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
        tokio::spawn(async move {
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
                    && Arc::ptr_eq(&cached.generation, &generation)
                {
                    cached.refreshing = false;
                    match result {
                        Ok(summary) => {
                            cached.lines =
                                summary.buckets.iter().map(ToString::to_string).collect();
                            cached.problem = None;
                            cached.note = None;
                            cached.checked_at = Some(inner.clock.now().to_string());
                            pools = Some(summary);
                        }
                        Err(QuotaError::NoCheck(why)) => cached.no_check(why),
                        Err(error) => cached.problem = Some(error.to_string()),
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
        });
    }
}

fn due(attempted: Option<Instant>, now: Instant, interval: Duration) -> bool {
    attempted.is_none_or(|at| now.saturating_duration_since(at) >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiPart, Conversation, Handled};
    use crate::routing::{ModelCatalog, StarterTiers};
    use crate::settings_api::{SettingsDeps, SettingsPart};
    use qm_auth::quota::QuotaEndpoints;
    use qm_auth::{
        AuthError, Credential, CredentialStore, LoginEndpoints, LoginHealth, Secret, TokenKeeper,
    };
    use qm_engine::DetectedModel;
    use qm_store::models::ModelFacts;
    use qm_types::api::{Command, SettingsView, Snapshot};
    use qm_types::clock::{Clock, Sleep};
    use qm_types::config::Config;
    use qm_types::data_dir::DataDir;
    use qm_types::domain::{AccountKind, Provider};
    use qm_types::ids::{AccountId, AgentId};
    use std::collections::BTreeMap;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::{mpsc, oneshot, watch};

    // There is no shared settings fixture or MockClock in this checkout.
    #[derive(Debug)]
    struct MockClock {
        base: Instant,
        elapsed: watch::Sender<Duration>,
        sleeps: AtomicUsize,
    }

    impl MockClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                elapsed: watch::channel(Duration::ZERO).0,
                sleeps: AtomicUsize::new(0),
            }
        }

        fn advance(&self, duration: Duration) {
            self.elapsed.send_modify(|elapsed| *elapsed += duration);
        }
    }

    impl Clock for MockClock {
        fn now(&self) -> jiff::Timestamp {
            "2026-10-23T00:00:00Z".parse::<jiff::Timestamp>().unwrap()
                + jiff::SignedDuration::from_secs(
                    i64::try_from(self.elapsed.borrow().as_secs()).unwrap(),
                )
        }

        fn instant(&self) -> Instant {
            self.base + *self.elapsed.borrow()
        }

        fn sleep_until(&self, deadline: Instant) -> Sleep {
            self.sleeps.fetch_add(1, Ordering::SeqCst);
            let base = self.base;
            let mut elapsed = self.elapsed.subscribe();
            Box::pin(async move {
                loop {
                    if base + *elapsed.borrow_and_update() >= deadline {
                        return;
                    }
                    if elapsed.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    #[derive(Debug, Default)]
    struct Store(Mutex<Option<Credential>>);

    impl CredentialStore for Store {
        fn get(&self, _: &CredentialRef) -> Result<Option<Credential>, AuthError> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn put(&self, _: &CredentialRef, credential: Credential) -> Result<(), AuthError> {
            *self.0.lock().unwrap() = Some(credential);
            Ok(())
        }
        fn remove(&self, _: &CredentialRef) -> Result<(), AuthError> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct EmptyCatalog;
    impl ModelCatalog for EmptyCatalog {
        fn resolve(&self, _: Provider, _: &DetectedModel) -> ModelFacts {
            panic!("quota checks must not resolve models")
        }
        fn catalog_list(&self, _: Provider) -> Vec<DetectedModel> {
            vec![]
        }
        fn starter_set(&self, _: Provider) -> StarterTiers {
            StarterTiers::default()
        }
    }

    type Reply = oneshot::Sender<(u16, &'static str)>;
    struct Fixture {
        part: SettingsPart,
        clock: Arc<MockClock>,
        store: Arc<Store>,
        quotas: Arc<Quotas>,
        requests: mpsc::UnboundedReceiver<Reply>,
        server: tokio::task::JoinHandle<()>,
        count: Arc<AtomicUsize>,
        dir: std::path::PathBuf,
        account: AccountId,
    }

    async fn eventually(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background quota operation did not finish");
    }

    async fn serve(
        listener: tokio::net::TcpListener,
        tx: mpsc::UnboundedSender<Reply>,
        count: Arc<AtomicUsize>,
    ) {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read only through the headers; never print the synthetic credential.
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if socket.read_exact(&mut byte).await.is_err() {
                    return;
                }
                headers.push(byte[0]);
            }
            count.fetch_add(1, Ordering::SeqCst);
            let (reply, response) = oneshot::channel();
            if tx.send(reply).is_err() {
                return;
            }
            let Ok((status, body)) = response.await else {
                return;
            };
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    }

    impl Fixture {
        async fn new() -> Self {
            Self::with(Provider::OpenRouter, None).await
        }

        async fn with(provider: Provider, base_url: Option<&str>) -> Self {
            let base_url = base_url.map(str::to_owned);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/quota", listener.local_addr().unwrap());
            let (tx, requests) = mpsc::unbounded_channel::<Reply>();
            let count = Arc::new(AtomicUsize::new(0));
            let server = tokio::spawn(serve(listener, tx, Arc::clone(&count)));
            let dir = std::env::temp_dir().join(format!(
                "qm-quota-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = qm_store::db::Db::open(&dir.join("state.db")).unwrap();
            let clock = Arc::new(MockClock::new());
            let at = clock.now();
            let account = db
                .call(move |conn| {
                    accounts::insert_account(
                        conn,
                        &accounts::NewAccount {
                            provider,
                            label: "Quota fixture".into(),
                            kind: AccountKind::ApiKey,
                            credential_ref: Some("test:quota".into()),
                            base_url,
                            at,
                        },
                    )
                })
                .await
                .unwrap();
            let store = Arc::new(Store(Mutex::new(Some(Credential::ApiKey {
                key: Secret::new("synthetic-quota-key"),
            }))));
            let tokens = Arc::new(TokenKeeper::new(
                store.clone(),
                LoginEndpoints::default(),
                Arc::new(LoginHealth::default()),
            ));
            // Every supported endpoint stays on loopback even if the fixture provider changes.
            let quotas = Arc::new(Quotas::with_endpoints(
                Arc::clone(&tokens),
                QuotaEndpoints::local(&url).unwrap(),
            ));
            let conversation = Arc::new(Conversation::new(
                db.clone(),
                clock.clone(),
                AgentId::new(1),
            ));
            let part = TEST_QUOTAS
                .scope(Arc::clone(&quotas), async {
                    SettingsPart::new(SettingsDeps {
                        db,
                        clock: clock.clone(),
                        data_dir: DataDir::new(&dir),
                        config: watch::channel(Config::default()).0,
                        tokens,
                        engines: BTreeMap::new(),
                        catalog: Arc::new(EmptyCatalog),
                        conversation,
                        endpoints: LoginEndpoints::default(),
                        quota_endpoints: QuotaEndpoints::default(),
                    })
                })
                .await;
            // Both background workers park on the clock: the quota checks and the usage feed.
            eventually(|| clock.sleeps.load(Ordering::SeqCst) == 2).await;
            Self {
                part,
                clock,
                store,
                quotas,
                requests,
                server,
                count,
                dir,
                account,
            }
        }

        async fn fill(&self) -> SettingsView {
            let mut snapshot = Snapshot::default();
            tokio::time::timeout(
                Duration::from_secs(5),
                TEST_QUOTAS.scope(Arc::clone(&self.quotas), self.part.fill(&mut snapshot)),
            )
            .await
            .expect("snapshot waited for provider HTTP");
            snapshot.settings
        }

        async fn command(&self, command: Command) {
            let handled = tokio::time::timeout(
                Duration::from_secs(5),
                TEST_QUOTAS.scope(
                    Arc::clone(&self.quotas),
                    self.part.handle(&command).expect("settings command"),
                ),
            )
            .await
            .expect("command waited for provider HTTP");
            assert!(matches!(handled, Handled::Changed));
        }

        async fn request(&mut self) -> Reply {
            tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
                .await
                .unwrap()
                .expect("mock HTTP server stopped")
        }

        async fn settled(&self) {
            eventually(|| {
                self.part
                    .inner
                    .lock()
                    .quotas
                    .values()
                    .all(|cached| !cached.refreshing)
            })
            .await;
        }

        async fn reply(&mut self, body: &'static str) {
            self.request().await.send((200, body)).unwrap();
            self.settled().await;
        }

        async fn success(&mut self) {
            self.request()
                .await
                .send((
                    200,
                    r#"{"data":{"limit":10,"limit_remaining":7,"usage":3}}"#,
                ))
                .unwrap();
            self.settled().await;
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn connected_account_snapshot_fetches_in_background_and_publishes_quota() {
        let mut f = Fixture::new().await;
        assert_eq!(f.count.load(Ordering::SeqCst), 0);
        let view = f.fill().await;
        let account = &view.accounts[0];
        assert_eq!(account.id, f.account);
        assert!(account.quota_refreshing);
        assert_eq!(account.quota_lines, Vec::<String>::new());
        assert!(account.quota_checked_at.is_none());
        f.success().await;
        let view = f.fill().await;
        let account = &view.accounts[0];
        assert_eq!(account.quota_lines, ["Key budget: $7.00 remaining"]);
        assert_eq!(account.quota_checked_at, Some(f.clock.now().to_string()));
        assert!(!account.quota_refreshing);
        assert!(account.quota_problem.is_none());
        assert_eq!(f.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_quota_check_feeds_routing_and_the_soft_limit_holds_the_account_back() {
        let mut f = Fixture::new().await;
        let view = f.fill().await;
        assert_eq!(
            view.accounts[0].routing.as_deref(),
            Some("quota unknown (pay-per-token keys serve last)")
        );
        // $3 of a $10 budget spent: routing reads 30% used.
        f.success().await;
        let view = f.fill().await;
        assert_eq!(
            view.accounts[0].routing.as_deref(),
            Some("prepaid budget 30% used")
        );
        f.command(Command::SetReserveMark {
            account: f.account,
            percent: Some(25),
        })
        .await;
        let view = f.fill().await;
        assert_eq!(view.accounts[0].reserve_at, Some(25));
        assert_eq!(
            view.accounts[0].routing.as_deref(),
            Some("prepaid budget 30% used · past its soft limit: others serve first")
        );
        f.command(Command::SetReserveMark {
            account: f.account,
            percent: None,
        })
        .await;
        assert_eq!(f.fill().await.accounts[0].reserve_at, None);
    }

    #[tokio::test]
    async fn a_deepseek_balance_is_measured_against_its_last_top_up() {
        let mut f = Fixture::with(Provider::Custom, Some("https://api.deepseek.com")).await;
        let routing = |view: &SettingsView| view.accounts[0].routing.clone().unwrap();
        f.fill().await;
        f.reply(
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"10.00"}]}"#,
        )
        .await;
        let view = f.fill().await;
        assert_eq!(view.accounts[0].quota_lines, ["Balance: $10.00"]);
        assert_eq!(routing(&view), "balance 0% used since the last top-up");
        f.command(Command::SetReserveMark {
            account: f.account,
            percent: Some(25),
        })
        .await;
        // $3 spent of the $10 it had: 30% used, past the mark.
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        f.reply(
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"7.00"}]}"#,
        )
        .await;
        assert_eq!(
            routing(&f.fill().await),
            "balance 30% used since the last top-up · past its soft limit: others serve first"
        );
        // A top-up to $8 is a new reference, though it is less than the first.
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        f.reply(
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"8.00"}]}"#,
        )
        .await;
        assert_eq!(
            routing(&f.fill().await),
            "balance 0% used since the last top-up"
        );
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        f.reply(
            r#"{"is_available":false,"balance_infos":[{"currency":"USD","total_balance":"0.00"}]}"#,
        )
        .await;
        assert_eq!(
            routing(&f.fill().await),
            "balance 100% used since the last top-up · at its limit: skipped until a check finds \
             money left"
        );
    }

    #[tokio::test]
    async fn a_moonshot_balance_follows_the_same_top_up_rule() {
        let mut f = Fixture::with(Provider::Custom, Some("https://api.moonshot.cn/v1")).await;
        let routing = |view: &SettingsView| view.accounts[0].routing.clone().unwrap();
        f.fill().await;
        f.reply(
            r#"{"code":0,"data":{"available_balance":20.0,"cash_balance":20.0},"status":true}"#,
        )
        .await;
        let view = f.fill().await;
        assert_eq!(view.accounts[0].quota_lines, ["Balance: CN¥20.00"]);
        assert_eq!(routing(&view), "balance 0% used since the last top-up");
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        f.reply(r#"{"code":0,"data":{"available_balance":5.0,"cash_balance":5.0},"status":true}"#)
            .await;
        assert_eq!(
            routing(&f.fill().await),
            "balance 75% used since the last top-up"
        );
    }

    #[tokio::test]
    async fn a_claude_or_keyless_account_gets_a_note_and_no_request() {
        for base_url in [None, Some("https://api.example.com/v1")] {
            let provider = if base_url.is_some() {
                Provider::Custom
            } else {
                Provider::Anthropic
            };
            let f = Fixture::with(provider, base_url).await;
            // Claude has no credential here at all: the app never reads its login.
            let at = f.clock.now();
            let claude = f
                .part
                .inner
                .db
                .call(move |conn| {
                    accounts::insert_account(
                        conn,
                        &accounts::NewAccount {
                            provider: Provider::Claude,
                            label: "Claude".into(),
                            kind: AccountKind::Subscription,
                            credential_ref: None,
                            base_url: None,
                            at,
                        },
                    )
                })
                .await
                .unwrap();
            for _ in 0..2 {
                let view = f.fill().await;
                for account in &view.accounts {
                    let note = if account.id == claude {
                        "not offered; Claude reports its usage during turns"
                    } else {
                        "not offered by this provider"
                    };
                    assert_eq!(account.quota_note.as_deref(), Some(note));
                    assert_eq!(account.quota_problem, None);
                    assert!(!account.quota_refreshing);
                    assert_eq!(account.quota_lines, [] as [String; 0]);
                    assert_eq!(account.quota_checked_at, None);
                }
                assert_eq!(view.accounts.len(), 2);
                // Neither the periodic worker nor the open cooldown ever asks.
                f.clock.advance(PERIOD);
                f.command(Command::RefreshQuotas).await;
            }
            assert_eq!(f.count.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn a_sign_in_with_chatgpt_account_gets_a_note_not_a_problem() {
        let f = Fixture::with(Provider::ChatGpt, None).await;
        // Sign in with ChatGPT's login has no account ID; `wham/usage` takes only the Codex one.
        *f.store.0.lock().unwrap() = Some(Credential::OAuth {
            access_token: Secret::new("synthetic-access"),
            refresh_token: Some(Secret::new("synthetic-refresh")),
            expires_at: None,
            account_id: None,
            client_id: Some("synthetic-client".into()),
        });
        f.fill().await;
        f.settled().await;
        let account = f.fill().await.accounts.remove(0);
        assert_eq!(
            account.quota_note.as_deref(),
            Some("not offered for Sign in with ChatGPT")
        );
        assert_eq!(account.quota_problem, None);
        assert!(!account.quota_refreshing);
        assert_eq!(account.quota_lines, [] as [String; 0]);
        assert_eq!(f.count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn refresh_command_is_nonblocking_coalesces_and_obeys_open_cooldown() {
        let mut f = Fixture::new().await;
        f.command(Command::RefreshQuotas).await;
        let held = f.request().await;
        assert!(f.fill().await.accounts[0].quota_refreshing);
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        assert_eq!(f.count.load(Ordering::SeqCst), 1);
        held.send((
            200,
            r#"{"data":{"limit":10,"limit_remaining":7,"usage":3}}"#,
        ))
        .unwrap();
        f.settled().await;
        // Cooldown is measured from the attempt, not its completion.
        f.command(Command::RefreshQuotas).await;
        f.success().await;
        f.clock
            .advance(OPEN_COOLDOWN.checked_sub(Duration::from_secs(1)).unwrap());
        f.command(Command::RefreshQuotas).await;
        assert!(!f.part.inner.lock().quotas[&f.account].refreshing);
        assert_eq!(f.count.load(Ordering::SeqCst), 2);
        f.clock.advance(Duration::from_secs(1));
        f.command(Command::RefreshQuotas).await;
        f.success().await;
        assert_eq!(f.count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn periodic_worker_and_snapshot_respect_five_minute_interval() {
        let mut f = Fixture::new().await;
        f.fill().await;
        f.success().await;
        f.clock
            .advance(PERIOD.checked_sub(Duration::from_secs(1)).unwrap());
        assert!(!f.fill().await.accounts[0].quota_refreshing);
        assert_eq!(f.count.load(Ordering::SeqCst), 1);
        f.clock.advance(Duration::from_secs(1));
        // No UI call: start()'s clock-driven worker must issue the request.
        f.success().await;
        assert_eq!(f.count.load(Ordering::SeqCst), 2);
        assert!(!f.fill().await.accounts[0].quota_refreshing);
    }

    #[tokio::test]
    async fn failed_refresh_preserves_last_success_and_later_success_clears_problem() {
        let mut f = Fixture::new().await;
        f.fill().await;
        f.success().await;
        let old = f.fill().await.accounts.remove(0);
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        let pending = f.fill().await.accounts.remove(0);
        assert!(pending.quota_refreshing);
        assert_eq!(pending.quota_lines, old.quota_lines);
        f.request()
            .await
            .send((429, "sensitive provider error body"))
            .unwrap();
        f.settled().await;
        let failed = f.fill().await.accounts.remove(0);
        assert_eq!(failed.quota_lines, old.quota_lines);
        assert_eq!(failed.quota_checked_at, old.quota_checked_at);
        assert_eq!(
            failed.quota_problem,
            Some(QuotaError::RateLimited.to_string())
        );
        assert!(!failed.quota_refreshing);
        f.clock.advance(OPEN_COOLDOWN);
        f.command(Command::RefreshQuotas).await;
        f.success().await;
        let recovered = f.fill().await.accounts.remove(0);
        assert!(recovered.quota_problem.is_none());
        assert_ne!(recovered.quota_checked_at, old.quota_checked_at);
    }

    #[tokio::test]
    async fn disconnect_prunes_cache_and_late_http_cannot_recreate_it() {
        let mut f = Fixture::new().await;
        f.fill().await;
        let held = f.request().await;
        f.command(Command::DisconnectAccount { account: f.account })
            .await;
        assert!(f.store.0.lock().unwrap().is_none());
        assert_eq!(f.fill().await.accounts, vec![]);
        assert!(f.part.inner.lock().quotas.is_empty());
        // A change notification proves the late request finished its cache-update path.
        held.send((
            200,
            r#"{"data":{"limit":10,"limit_remaining":7,"usage":3}}"#,
        ))
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), f.part.inner.conversation.changes())
            .await
            .unwrap();
        assert!(f.part.inner.lock().quotas.is_empty());
        assert_eq!(f.fill().await.accounts, vec![]);
        assert_eq!(f.count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconnect_refreshes_immediately_and_discards_previous_credential_response() {
        let mut f = Fixture::new().await;
        f.fill().await;
        let held = f.request().await;
        let row = f
            .part
            .inner
            .db
            .call(|c| Ok(accounts::list_active(c)?.remove(0)))
            .await
            .unwrap();
        // The fixture has no model engine, but credentials are replaced before model detection.
        let _ = f
            .part
            .inner
            .reconnect(
                row,
                Credential::ApiKey {
                    key: Secret::new("synthetic-replacement-key"),
                },
            )
            .await;
        f.part.inner.conversation.changes().await;
        let fresh = f.fill().await;
        assert!(fresh.accounts[0].quota_refreshing);
        assert_eq!(fresh.accounts[0].quota_lines, Vec::<String>::new());
        held.send((200, r#"{"data":{"limit":10,"limit_remaining":1}}"#))
            .unwrap();
        let replacement = f.request().await;
        f.part.inner.conversation.changes().await;
        let pending = f.fill().await;
        assert!(pending.accounts[0].quota_refreshing);
        assert_eq!(pending.accounts[0].quota_lines, Vec::<String>::new());
        replacement
            .send((200, r#"{"data":{"limit":10,"limit_remaining":9}}"#))
            .unwrap();
        f.settled().await;
        assert_eq!(
            f.fill().await.accounts[0].quota_lines,
            ["Key budget: $9.00 remaining"]
        );
    }

    #[test]
    fn first_connection_periodic_refresh_and_open_cooldown() {
        let now = Instant::now();
        assert!(due(None, now, PERIOD));
        assert!(!due(Some(now), now, OPEN_COOLDOWN));
        assert!(due(Some(now), now + OPEN_COOLDOWN, OPEN_COOLDOWN));
        assert!(!due(Some(now), now + OPEN_COOLDOWN, PERIOD));
        assert!(due(Some(now), now + PERIOD, PERIOD));
    }
}
