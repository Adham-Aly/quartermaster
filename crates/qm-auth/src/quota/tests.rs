use super::*;
use crate::{AuthError, CredentialStore, LoginEndpoints, LoginHealth};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[derive(Debug, Default)]
struct Store {
    credential: Mutex<Option<Credential>>,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl CredentialStore for Store {
    fn get(&self, _: &CredentialRef) -> Result<Option<Credential>, AuthError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.credential.lock().unwrap().clone())
    }
    fn put(&self, _: &CredentialRef, credential: Credential) -> Result<(), AuthError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        *self.credential.lock().unwrap() = Some(credential);
        Ok(())
    }
    fn remove(&self, _: &CredentialRef) -> Result<(), AuthError> {
        *self.credential.lock().unwrap() = None;
        Ok(())
    }
}

fn oauth() -> Credential {
    Credential::OAuth {
        access_token: Secret::new("test-access"),
        refresh_token: Some(Secret::new("test-github-or-refresh")),
        expires_at: None,
        account_id: Some("test-account".into()),
        client_id: Some("test-client".into()),
    }
}

fn key() -> CredentialRef {
    CredentialRef::new("test:quota").unwrap()
}

fn setup(
    credential: Option<Credential>,
    endpoints: LoginEndpoints,
) -> (Arc<Store>, Arc<TokenKeeper>) {
    let store = Arc::new(Store {
        credential: Mutex::new(credential),
        ..Store::default()
    });
    let keeper = Arc::new(TokenKeeper::new(
        store.clone(),
        endpoints,
        Arc::new(LoginHealth::default()),
    ));
    (store, keeper)
}

// Captured requests contain only synthetic test credentials and never get printed.
async fn server(replies: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    gated_server(replies, None).await
}

type ResponseGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

async fn gated_server(
    replies: Vec<(u16, String)>,
    mut gate: Option<ResponseGate>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/quota", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 2048];
                let count = socket.read(&mut buf).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..count]);
                if request.windows(4).any(|s| s == b"\r\n\r\n") {
                    break;
                }
            }
            requests.push(String::from_utf8(request).unwrap().to_lowercase());
            if let Some((started, release)) = gate.take() {
                started.send(()).unwrap();
                release.await.unwrap();
            }
            let reply = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        requests
    });
    (url, task)
}

fn endpoints(url: &str) -> QuotaEndpoints {
    QuotaEndpoints::local(url).unwrap()
}

const CHATGPT: &str = r#"{"rate_limit":{"primary_window":{"used_percent":25,"reset_at":2000000000,"limit_window_seconds":18000},"secondary_window":{"used_percent":100}},"email":"must-not-escape","plan_type":"\u001b[31muntrusted"}"#;

#[test]
fn chatgpt_windows_are_safe_and_numeric() {
    let summary = parse_chatgpt(&serde_json::from_str(CHATGPT).unwrap()).unwrap();
    assert_eq!(summary.buckets.len(), 2);
    assert_eq!(summary.buckets[0].remaining_percent, Some(75.0));
    assert_eq!(summary.buckets[0].window_seconds, Some(18000));
    assert_eq!(
        summary.buckets[0].resets_at.unwrap().as_second(),
        2_000_000_000
    );
    assert_eq!(summary.buckets[1].remaining_percent, Some(0.0));
    let display = format!("{summary:?} {}", summary.buckets[0]);
    assert!(!display.contains("must-not-escape"));
    assert!(!display.contains("untrusted"));
    assert!(!display.contains('\u{1b}'));
}

#[test]
fn copilot_handles_unlimited_and_missing_fields() {
    let summary = parse_copilot(&json!({
        "quota_reset_date": "2030-01-01T00:00:00Z",
        "quota_snapshots": {
            "chat": {"unlimited": true},
            "premium_interactions": {"remaining": 42, "entitlement": 100, "percent_remaining": 42.0},
            "untrusted": {"remaining": 999}
        }
    })).unwrap();
    assert_eq!(summary.buckets.len(), 2);
    assert!(summary.buckets[0].unlimited);
    assert_eq!(summary.buckets[0].remaining_percent, None);
    assert!(summary.buckets[0].resets_at.is_some());
    assert_eq!(summary.buckets[1].remaining, Some(42));
    assert_eq!(summary.buckets[1].entitlement, Some(100));
    let date_only = parse_copilot(
        &json!({"quota_reset_date": "2030-01-01", "quota_snapshots": {"chat": {"remaining": 0}}}),
    )
    .unwrap();
    assert!(date_only.buckets[0].resets_at.is_none());
    assert_eq!(
        date_only.buckets[0].resets_on.unwrap().to_string(),
        "2030-01-01"
    );
    assert!(
        date_only.buckets[0]
            .to_string()
            .contains("resets 2030-01-01")
    );
}

#[test]
fn openrouter_reports_only_key_budget_not_account_credit() {
    let summary = parse_openrouter(
        &json!({"data": {"limit": 10, "limit_remaining": 2.5, "label": "test-key\u{1b}[31m"}}),
    )
    .unwrap();
    assert_eq!(summary.buckets[0].remaining_usd, Some(2.5));
    assert_eq!(summary.buckets[0].remaining_percent, Some(25.0));
    assert_eq!(
        summary.buckets[0].to_string(),
        "Key budget: $2.50 remaining"
    );
    let unlimited =
        parse_openrouter(&json!({"data": {"limit": null, "limit_remaining": null}})).unwrap();
    assert!(unlimited.buckets[0].unlimited);
    assert_eq!(unlimited.buckets[0].remaining_usd, None);
    for value in [
        json!({}),
        json!({"data": {"limit": 1}}),
        json!({"data": {"limit": -1, "limit_remaining": 0}}),
        json!({"data": {"limit": 1, "limit_remaining": "test-key"}}),
    ] {
        assert_eq!(parse_openrouter(&value), Err(QuotaError::InvalidResponse));
    }
}

#[tokio::test]
async fn openrouter_uses_api_key_without_editor_or_account_headers() {
    let (url, task) = server(vec![(
        200,
        json!({"data": {"limit": 10, "limit_remaining": 2.5}}).to_string(),
    )])
    .await;
    let (_, keeper) = setup(
        Some(Credential::ApiKey {
            key: Secret::new("test-key"),
        }),
        LoginEndpoints::default(),
    );
    let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
    assert!(
        quotas
            .fetch(Provider::OpenRouter, &key(), None)
            .await
            .is_ok()
    );
    let requests = task.await.unwrap();
    assert!(requests[0].contains("authorization: bearer test-key"));
    assert!(!requests[0].contains("copilot-integration-id"));
    assert!(!requests[0].contains("chatgpt-account-id"));
}

#[test]
fn rejects_missing_malformed_and_out_of_range_quota() {
    for value in [
        json!({}),
        json!([]),
        json!({"rate_limit": {"primary_window": {"used_percent": 101}}}),
        json!({"rate_limit": {"primary_window": {"used_percent": -1}}}),
        json!({"rate_limit": {"primary_window": {"used_percent": "test-access"}}}),
    ] {
        assert_eq!(parse_chatgpt(&value), Err(QuotaError::InvalidResponse));
    }
    assert_eq!(
        parse_copilot(
            &json!({"quota_snapshots": {"chat": {"remaining": -1, "percent_remaining": 101}}})
        ),
        Err(QuotaError::InvalidResponse)
    );
}

#[tokio::test]
async fn accounts_without_a_quota_check_never_read_credentials() {
    let (store, keeper) = setup(None, LoginEndpoints::default());
    let quotas = Quotas::new(keeper);
    assert_eq!(
        quotas.fetch(Provider::Claude, &key(), None).await,
        Err(QuotaError::NoCheck(NoQuotaCheck::Claude))
    );
    for provider in [
        Provider::Xai,
        Provider::Kimi,
        Provider::Gemini,
        Provider::Anthropic,
        Provider::OpenAi,
        Provider::Custom,
        Provider::Scripted,
    ] {
        assert_eq!(
            quotas.fetch(provider, &key(), None).await,
            Err(QuotaError::NoCheck(NoQuotaCheck::NoSource))
        );
    }
    // A custom endpoint anywhere but a balance source's API host has no quota source either.
    for base in [
        None,
        Some("https://api.example.com/v1"),
        Some("http://api.deepseek.com"),
        Some("https://api.deepseek.com.example.com"),
        Some("https://api.deepseek.com:8443"),
        Some("not a url"),
    ] {
        assert_eq!(
            quotas.fetch(Provider::Custom, &key(), base).await,
            Err(QuotaError::NoCheck(NoQuotaCheck::NoSource))
        );
    }
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn no_quota_check_reads_as_a_neutral_note() {
    for (why, text) in [
        (
            NoQuotaCheck::Claude,
            "not offered; Claude reports its usage during turns",
        ),
        (
            NoQuotaCheck::SignInWithChatGpt,
            "not offered for Sign in with ChatGPT",
        ),
        (NoQuotaCheck::NoSource, "not offered by this provider"),
    ] {
        assert_eq!(why.to_string(), text);
        assert_eq!(QuotaError::NoCheck(why).to_string(), text);
    }
}

#[tokio::test]
async fn no_check_for_matches_fetch() {
    let providers = [
        Provider::Claude,
        Provider::ChatGpt,
        Provider::Copilot,
        Provider::OpenRouter,
        Provider::Xai,
        Provider::Kimi,
        Provider::Gemini,
        Provider::Anthropic,
        Provider::OpenAi,
        Provider::Custom,
        Provider::Scripted,
    ];
    let balance_hosts = [
        "https://api.deepseek.com",
        "https://api.moonshot.ai/v1",
        "https://api.moonshot.cn/v1",
    ];
    let bases = [
        None,
        Some(balance_hosts[0]),
        Some(balance_hosts[1]),
        Some(balance_hosts[2]),
        Some("https://api.example.com/v1"),
        Some("http://api.deepseek.com"),
        Some("not a url"),
    ];
    for provider in providers {
        for base in bases {
            let checked = matches!(
                provider,
                Provider::ChatGpt | Provider::Copilot | Provider::OpenRouter
            ) || (provider == Provider::Custom
                && base.is_some_and(|b| balance_hosts.contains(&b)));
            assert_eq!(
                no_check_for(provider, base).is_none(),
                checked,
                "{provider:?} at {base:?}"
            );
            // Where there is no check, fetch says so without reading the store; where there is
            // one, it goes on to read the (here missing) credential.
            let (store, keeper) = setup(None, LoginEndpoints::default());
            let result = Quotas::new(keeper).fetch(provider, &key(), base).await;
            if let Some(why) = no_check_for(provider, base) {
                assert_eq!(result, Err(QuotaError::NoCheck(why)));
                assert_eq!(store.reads.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(result, Err(QuotaError::Credentials));
                assert!(store.reads.load(Ordering::SeqCst) > 0);
            }
        }
    }
}

#[tokio::test]
async fn sign_in_with_chatgpt_is_never_sent_or_refreshed_by_a_quota_check() {
    // No replies: both listeners close at once, so a request or refresh would fail loudly.
    let (url, task) = server(vec![]).await;
    let (token_url, refresh_task) = server(vec![]).await;
    let mut login = LoginEndpoints::default();
    login.chatgpt.token_url.clone_from(&token_url);
    login.chatgpt.codex_token_url = token_url;
    let mut credential = oauth();
    if let Credential::OAuth {
        account_id,
        expires_at,
        ..
    } = &mut credential
    {
        // Sign in with ChatGPT's credential has no account ID. It is expired, so `fresh()`
        // would refresh it if a quota check ever asked.
        *account_id = None;
        *expires_at = Some(Timestamp::UNIX_EPOCH);
    }
    let (store, keeper) = setup(Some(credential), login);
    let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
    for _ in 0..2 {
        assert_eq!(
            quotas.fetch(Provider::ChatGpt, &key(), None).await,
            Err(QuotaError::NoCheck(NoQuotaCheck::SignInWithChatGpt))
        );
    }
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert_eq!(task.await.unwrap(), [] as [String; 0]);
    assert_eq!(refresh_task.await.unwrap(), [] as [String; 0]);
}

#[test]
fn balances_are_found_by_the_custom_endpoints_api_host_only() {
    let custom = |url| BalanceSource::of(Provider::Custom, Some(url));
    assert_eq!(
        custom("https://api.deepseek.com"),
        Some(BalanceSource::DeepSeek)
    );
    assert_eq!(
        custom("https://api.deepseek.com/v1"),
        Some(BalanceSource::DeepSeek)
    );
    assert_eq!(
        custom("https://API.DeepSeek.com/"),
        Some(BalanceSource::DeepSeek)
    );
    assert_eq!(
        custom("https://api.moonshot.ai/v1"),
        Some(BalanceSource::Moonshot)
    );
    assert_eq!(
        custom("https://api.moonshot.cn/v1"),
        Some(BalanceSource::MoonshotChina)
    );
    for url in [
        "https://deepseek.com",
        "https://user:pw@api.deepseek.com",
        "http://api.moonshot.ai/v1",
        "https://api.moonshot.ai:444/v1",
        "https://api.kimi.com/coding/v1",
    ] {
        assert_eq!(custom(url), None, "{url}");
    }
    assert_eq!(BalanceSource::of(Provider::Custom, None), None);
    // Every other provider: no balance an inference key can read, whatever the URL.
    for provider in [
        Provider::Anthropic,
        Provider::OpenAi,
        Provider::Gemini,
        Provider::Xai,
        Provider::OpenRouter,
        Provider::Kimi,
    ] {
        assert_eq!(
            BalanceSource::of(provider, Some("https://api.deepseek.com")),
            None
        );
    }
}

#[test]
fn moonshot_balance_reads_the_available_balance_in_its_platforms_currency() {
    let value = json!({"code": 0, "data": {"available_balance": 49.58894,
        "voucher_balance": 46.58893, "cash_balance": 3.00001}, "scode": "0x0", "status": true});
    let usd = BalanceSource::Moonshot.parse(&value).unwrap();
    assert_eq!(usd.buckets[0].to_string(), "Balance: $49.59");
    let cny = BalanceSource::MoonshotChina.parse(&value).unwrap();
    assert_eq!(cny.buckets[0].to_string(), "Balance: CN¥49.59");
    // Owing money (a negative cash balance and no vouchers) is no balance left.
    let owing = parse_moonshot(
        &json!({"code": 0, "data": {"available_balance": -1.5}}),
        Currency::Usd,
    )
    .unwrap();
    assert_eq!(owing.buckets[0].balance.map(|m| m.amount), Some(0.0));
    for value in [
        json!({}),
        json!({"code": 0, "data": {}}),
        json!({"code": 0, "data": {"available_balance": "lots"}}),
        json!({"code": 1, "data": {"available_balance": 5.0}}),
    ] {
        assert_eq!(
            parse_moonshot(&value, Currency::Usd),
            Err(QuotaError::InvalidResponse)
        );
    }
}

#[test]
fn deepseek_balance_reads_allow_listed_currencies_only() {
    let summary = parse_deepseek(&json!({
        "is_available": true,
        "balance_infos": [
            {"currency": "CNY", "total_balance": "110.00", "granted_balance": "10.00",
             "topped_up_balance": "100.00"},
            {"currency": "\u{1b}[31mEUR", "total_balance": "5.00"},
            {"currency": "USD", "total_balance": 2.5},
        ]
    }))
    .unwrap();
    let lines: Vec<String> = summary.buckets.iter().map(ToString::to_string).collect();
    assert_eq!(lines, ["Balance: CN¥110.00", "Balance: $2.50"]);
    assert!(
        summary
            .buckets
            .iter()
            .all(|b| b.remaining_percent.is_none())
    );
    // Owing money is no balance left.
    let owing = parse_deepseek(&json!({"balance_infos": [
        {"currency": "USD", "total_balance": "-0.40"}
    ]}))
    .unwrap();
    assert_eq!(owing.buckets[0].balance.map(|m| m.amount), Some(0.0));
    for value in [
        json!({}),
        json!({"balance_infos": []}),
        json!({"balance_infos": [{"currency": "EUR", "total_balance": "1.00"}]}),
        json!({"balance_infos": [{"currency": "USD", "total_balance": "lots"}]}),
        json!({"balance_infos": [{"currency": "USD"}]}),
        json!({"balance_infos": [
            {"currency": "USD", "total_balance": "1.00"},
            {"currency": "USD", "total_balance": "2.00"}
        ]}),
    ] {
        assert_eq!(parse_deepseek(&value), Err(QuotaError::InvalidResponse));
    }
}

#[tokio::test]
async fn deepseek_reads_the_balance_with_its_api_key() {
    let (url, task) = server(vec![(
        200,
        json!({"is_available": true, "balance_infos": [
            {"currency": "USD", "total_balance": "7.25"}
        ]})
        .to_string(),
    )])
    .await;
    let (_, keeper) = setup(
        Some(Credential::ApiKey {
            key: Secret::new("test-key"),
        }),
        LoginEndpoints::default(),
    );
    let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
    let summary = quotas
        .fetch(
            Provider::Custom,
            &key(),
            Some("https://api.deepseek.com/v1"),
        )
        .await
        .unwrap();
    assert_eq!(summary.buckets[0].to_string(), "Balance: $7.25");
    let requests = task.await.unwrap();
    assert!(requests[0].contains("authorization: bearer test-key"));
    assert!(!requests[0].contains("chatgpt-account-id"));
}

#[tokio::test]
async fn each_balance_source_asks_its_own_fixed_url() {
    let moonshot = json!({"code": 0, "data": {"available_balance": 3.0}}).to_string();
    let deepseek =
        json!({"balance_infos": [{"currency": "CNY", "total_balance": "4.00"}]}).to_string();
    let (url, task) = server(vec![
        (200, deepseek),
        (200, moonshot.clone()),
        (200, moonshot),
    ])
    .await;
    let (_, keeper) = setup(
        Some(Credential::ApiKey {
            key: Secret::new("test-key"),
        }),
        LoginEndpoints::default(),
    );
    let mut endpoints = endpoints(&url);
    endpoints.deepseek = format!("{url}/deepseek");
    endpoints.moonshot = format!("{url}/moonshot-ai");
    endpoints.moonshot_cn = format!("{url}/moonshot-cn");
    let quotas = Quotas::with_endpoints(keeper, endpoints);
    let mut lines = Vec::new();
    for base in [
        "https://api.deepseek.com",
        "https://api.moonshot.ai/v1",
        "https://api.moonshot.cn/v1",
    ] {
        let summary = quotas
            .fetch(Provider::Custom, &key(), Some(base))
            .await
            .unwrap();
        lines.push(summary.buckets[0].to_string());
    }
    assert_eq!(
        lines,
        ["Balance: CN¥4.00", "Balance: $3.00", "Balance: CN¥3.00"]
    );
    let requests = task.await.unwrap();
    for (request, path) in requests
        .iter()
        .zip(["deepseek", "moonshot-ai", "moonshot-cn"])
    {
        assert!(
            request.starts_with(&format!("get /quota/{path} ")),
            "{path}"
        );
    }
}

#[tokio::test]
async fn uses_correct_tokens_and_headers() {
    for provider in [Provider::ChatGpt, Provider::Copilot] {
        let body = if provider == Provider::ChatGpt {
            CHATGPT
        } else {
            r#"{"quota_snapshots":{"chat":{"unlimited":true}}}"#
        };
        let (url, task) = server(vec![(200, body.into())]).await;
        let (_, keeper) = setup(Some(oauth()), LoginEndpoints::default());
        let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
        assert!(quotas.fetch(provider, &key(), None).await.is_ok());
        let requests = task.await.unwrap();
        let request = &requests[0];
        assert!(request.starts_with("get /quota "));
        if provider == Provider::ChatGpt {
            assert!(request.contains("authorization: bearer test-access"));
            assert!(request.contains("chatgpt-account-id: test-account"));
        } else {
            assert!(request.contains("authorization: bearer test-github-or-refresh"));
            assert!(!request.contains("test-access"));
            assert!(request.contains("copilot-integration-id: vscode-chat"));
        }
    }
}

#[tokio::test]
async fn errors_are_fixed_and_do_not_echo_bodies_or_poison_login_health() {
    for (status, expected) in [
        (401, QuotaError::Refused),
        (403, QuotaError::Refused),
        (429, QuotaError::RateLimited),
        (500, QuotaError::Unavailable),
        (302, QuotaError::Unavailable),
        (200, QuotaError::InvalidResponse),
    ] {
        let (url, task) = server(vec![(
            status,
            "test-access\u{1b}[31m test-github-or-refresh".into(),
        )])
        .await;
        let (_, keeper) = setup(Some(oauth()), LoginEndpoints::default());
        let quotas = Quotas::with_endpoints(keeper.clone(), endpoints(&url));
        let error = quotas
            .fetch(Provider::Copilot, &key(), None)
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("test-access"));
        assert!(!shown.contains("test-github-or-refresh"));
        assert!(!shown.contains('\u{1b}'));
        assert!(keeper.health().is_usable(&key()));
        task.await.unwrap();
    }
}

/// A synthetic Codex access token: an unsigned JWT whose only claim is the account ID.
fn codex_jwt() -> String {
    use base64::Engine as _;
    let encode =
        |value: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string());
    let claims =
        json!({ crate::chatgpt::JWT_CLAIM_PATH: { "chatgpt_account_id": "test-account" } });
    format!(
        "{}.{}.test-signature",
        encode(&json!({"alg": "none"})),
        encode(&claims)
    )
}

/// The Codex token endpoint's answer to a refresh.
fn codex_refresh_reply() -> String {
    json!({ "access_token": codex_jwt(), "refresh_token": "test-rotated", "expires_in": 3600 })
        .to_string()
}

#[tokio::test]
async fn refreshes_before_expiry_or_after_401_persisting_and_retrying_only_once() {
    let renewed = codex_jwt().to_lowercase();
    for (expired, final_status) in [(true, 200), (false, 200), (false, 401)] {
        let mut replies = Vec::new();
        if !expired {
            replies.push((401, "ignored".into()));
        }
        replies.push((final_status, CHATGPT.into()));
        let (url, task) = server(replies).await;
        let (token_url, refresh_task) = server(vec![(200, codex_refresh_reply())]).await;
        let mut login = LoginEndpoints::default();
        login.chatgpt.token_url.clone_from(&token_url);
        // Both refresh flows stay on loopback, including if a fixture changes account ID.
        login.chatgpt.codex_token_url = token_url;
        // The Codex login (it has an account ID): the only ChatGPT login a quota check sends.
        let mut credential = oauth();
        if expired && let Credential::OAuth { expires_at, .. } = &mut credential {
            *expires_at = Some(Timestamp::UNIX_EPOCH);
        }
        let (store, keeper) = setup(Some(credential), login);
        let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
        let result = quotas.fetch(Provider::ChatGpt, &key(), None).await;
        if final_status == 200 {
            assert!(result.is_ok());
        } else {
            assert_eq!(result, Err(QuotaError::Refused));
        }
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), if expired { 1 } else { 2 });
        assert!(
            requests
                .last()
                .unwrap()
                .contains(&format!("authorization: bearer {renewed}"))
        );
        assert!(
            requests
                .last()
                .unwrap()
                .contains("chatgpt-account-id: test-account")
        );
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        let stored = store.get(&key()).unwrap().unwrap();
        assert!(
            matches!(stored, Credential::OAuth { refresh_token: Some(ref token), .. } if token.expose() == "test-rotated")
        );
        refresh_task.await.unwrap();
    }
}

#[tokio::test]
async fn missing_and_api_key_credentials_are_sanitized() {
    for (credential, expected) in [
        (None, QuotaError::Credentials),
        (
            Some(Credential::ApiKey {
                key: Secret::new("test-key"),
            }),
            QuotaError::NoCheck(NoQuotaCheck::NoSource),
        ),
    ] {
        let (_, keeper) = setup(credential, LoginEndpoints::default());
        assert_eq!(
            Quotas::new(keeper)
                .fetch(Provider::ChatGpt, &key(), None)
                .await,
            Err(expected)
        );
    }
}

#[test]
fn endpoints_require_https_or_loopback() {
    for endpoint in [
        "http://example.com/quota",
        "file:///quota",
        "https://user:password@example.com/quota",
        "not a url",
    ] {
        assert_eq!(validate_endpoint(endpoint), Err(QuotaError::Unavailable));
    }
    assert!(validate_endpoint("https://chatgpt.com/backend-api/wham/usage").is_ok());
    assert!(validate_endpoint("http://127.0.0.1:123/quota").is_ok());
}

#[test]
fn local_endpoints_stay_on_this_machine() {
    let local = QuotaEndpoints::local("http://127.0.0.1:41010/quota").unwrap();
    assert_eq!(local.deepseek, "http://127.0.0.1:41010/quota");
    assert!(QuotaEndpoints::local("http://localhost:1/q").is_some());
    for url in [
        "https://api.deepseek.com/user/balance",
        "http://127.0.0.1.example.com/q",
        "file:///q",
        "not a url",
    ] {
        assert!(QuotaEndpoints::local(url).is_none(), "{url}");
    }
}

#[tokio::test]
async fn network_failure_is_fixed_and_oversized_body_is_rejected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/test-access", listener.local_addr().unwrap());
    drop(listener);
    let (_, keeper) = setup(Some(oauth()), LoginEndpoints::default());
    let quotas = Quotas::with_endpoints(keeper.clone(), endpoints(&url));
    assert_eq!(
        quotas.fetch(Provider::Copilot, &key(), None).await,
        Err(QuotaError::Unavailable)
    );
    let (url, task) = server(vec![(200, " ".repeat(1024 * 1024 + 1))]).await;
    let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
    assert_eq!(
        quotas.fetch(Provider::Copilot, &key(), None).await,
        Err(QuotaError::InvalidResponse)
    );
    task.await.unwrap();
}

#[tokio::test]
async fn refuses_redirects_even_to_same_origin() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/quota", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buf = [0; 4096];
            let count = socket.read(&mut buf).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
            if request.windows(4).any(|s| s == b"\r\n\r\n") {
                break;
            }
        }
        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        drop(socket);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let (_, keeper) = setup(Some(oauth()), LoginEndpoints::default());
    let quotas = Quotas::with_endpoints(keeper, endpoints(&url));
    assert_eq!(
        quotas.fetch(Provider::Copilot, &key(), None).await,
        Err(QuotaError::Unavailable)
    );
    task.await.unwrap();
}

#[test]
fn displays_window_durations_and_generic_fallbacks() {
    for (kind, seconds, expected) in [
        (QuotaKind::Primary, Some(18_000), "5-hour"),
        (QuotaKind::Secondary, Some(604_800), "7-day"),
        (QuotaKind::Primary, Some(90), "90-second"),
        (QuotaKind::Secondary, Some(1_800), "30-minute"),
        (QuotaKind::Primary, None, "Primary"),
        (QuotaKind::Secondary, Some(0), "Secondary"),
        (QuotaKind::Chat, Some(18_000), "Chat"),
    ] {
        let mut b = bucket(kind);
        b.window_seconds = seconds;
        b.remaining_percent = Some(75.0);
        assert_eq!(b.to_string(), format!("{expected}: 75% remaining"));
    }
}

#[tokio::test]
async fn copilot_quota_succeeds_even_when_expired_inference_exchange_fails() {
    let (token_url, exchange_task) = server(vec![(500, "unavailable".into())]).await;
    let (quota_url, quota_task) = server(vec![(
        200,
        r#"{"quota_snapshots":{"chat":{"unlimited":true}}}"#.into(),
    )])
    .await;
    let mut login = LoginEndpoints::default();
    login.copilot.copilot_token_url = token_url;
    let mut credential = oauth();
    if let Credential::OAuth { expires_at, .. } = &mut credential {
        *expires_at = Some(Timestamp::UNIX_EPOCH);
    }
    let (store, keeper) = setup(Some(credential), login);
    assert!(keeper.fresh(Provider::Copilot, &key(), None).await.is_err());
    exchange_task.await.unwrap();
    let quotas = Quotas::with_endpoints(keeper.clone(), endpoints(&quota_url));
    assert!(quotas.fetch(Provider::Copilot, &key(), None).await.is_ok());
    let requests = quota_task.await.unwrap();
    assert!(requests[0].contains("authorization: bearer test-github-or-refresh"));
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert!(keeper.health().is_usable(&key()));
}

#[tokio::test]
async fn disconnect_waits_for_quota_refresh_and_queued_refresh_cannot_restore_secret() {
    use std::{future::Future as _, task::Poll};

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (token_url, refresh_task) = gated_server(
        vec![(200, codex_refresh_reply())],
        Some((started_tx, release_rx)),
    )
    .await;
    let (quota_url, quota_task) = server(vec![(200, CHATGPT.into())]).await;
    let mut login = LoginEndpoints::default();
    login.chatgpt.token_url.clone_from(&token_url);
    login.chatgpt.codex_token_url = token_url;
    // The Codex login: a Sign in with ChatGPT login is never refreshed by a quota check.
    let mut credential = oauth();
    if let Credential::OAuth { expires_at, .. } = &mut credential {
        *expires_at = Some(Timestamp::UNIX_EPOCH);
    }
    let (store, keeper) = setup(Some(credential), login);
    let quotas = Quotas::with_endpoints(keeper.clone(), endpoints(&quota_url));
    let fetch = tokio::spawn(async move { quotas.fetch(Provider::ChatGpt, &key(), None).await });
    started_rx.await.unwrap();
    let key = key();
    let forget = keeper.forget(&key);
    tokio::pin!(forget);
    // Poll explicitly to prove Disconnect is blocked behind the held refresh, without sleeps.
    std::future::poll_fn(|cx| {
        assert!(forget.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let queued = keeper.fresh(Provider::ChatGpt, &key, None);
    tokio::pin!(queued);
    std::future::poll_fn(|cx| {
        assert!(queued.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    release_tx.send(()).unwrap();
    forget.await.unwrap();
    assert!(store.get(&key).unwrap().is_none());
    assert!(matches!(queued.await, Err(AuthError::Rejected(_))));
    assert!(fetch.await.unwrap().is_ok());
    refresh_task.await.unwrap();
    quota_task.await.unwrap();
    assert!(store.get(&key).unwrap().is_none());
    assert_eq!(store.writes.load(Ordering::SeqCst), 1);
}
