//! Quota-aware routing through the store: the readings on each account's row (engine windows and
//! quota checks) and its soft limit decide which account in the tier serves.

use super::*;
use qm_policy::overrides::Authority;
use qm_store::{connection, schema, seat};
use qm_types::domain::StartedBy;
use qm_types::ids::MessageId;
use rusqlite::params;

const CLAUDE: i64 = 201;
const CHATGPT: i64 = 202;
const DEEPSEEK: i64 = 203;

fn now() -> Timestamp {
    "2026-10-23T00:00:00Z".parse().unwrap()
}

// Three accounts, one High model each: a Claude subscription, a ChatGPT subscription, and a
// DeepSeek key on a custom endpoint. Placement order puts DeepSeek first and Claude last, so
// only quota can make Claude win.
fn fixture() -> (Connection, TaskId) {
    let mut conn = connection::open_memory().unwrap();
    schema::migrate(&mut conn).unwrap();
    let dir = std::env::temp_dir();
    let qm = seat::ensure_quartermaster(&conn, &dir, now()).unwrap().id;
    conn.execute_batch(
        r#"
        INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq) VALUES
          (201, 'claude', 'Claude Max', 'subscription', 'claude', 0, 10),
          (202, 'chatgpt', 'ChatGPT Pro', 'subscription', 'provider', 0, 11),
          (203, 'custom', 'DeepSeek', 'api_key', 'provider', 0, 12);
        INSERT INTO models (account_id, model, canonical, family, display_name, capabilities,
            efforts, first_seen_at, is_new, seq) VALUES
          (201, 'claude-opus-5-5', 'claude-opus-5-5', 'claude-opus', 'Opus',
           '{"image_input":true,"context_window":1000000,"generation":1,"known":true,"source":"account"}',
           '["low","medium","high"]', 0, 0, 20),
          (202, 'gpt-6', 'gpt-6', 'gpt-6', 'GPT-6',
           '{"image_input":true,"context_window":400000,"generation":1,"known":true,"source":"account"}',
           '["low","medium","high"]', 0, 0, 21),
          (203, 'deepseek-v4', 'deepseek-v4', 'deepseek', 'DeepSeek V4',
           '{"image_input":true,"context_window":400000,"generation":1,"known":true,"source":"account"}',
           '["low","medium","high"]', 0, 0, 22);
        INSERT INTO placements (canonical, tier, effort_override, created_at, seq) VALUES
          ('deepseek-v4', 'high', 'high', 0, 30),
          ('gpt-6', 'high', 'high', 0, 31),
          ('claude-opus-5-5', 'high', 'high', 0, 32);
        INSERT INTO projects (name, path, origin, default_branch, delivery_mode,
            merge_method, created_at, seq)
          VALUES ('p', '/tmp/p', 'linked', 'main', 'direct_pr', 'merge', 0, 40);
        "#,
    )
    .unwrap();
    let project = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO user_message_sources (id, agent_id, created_at, seq) VALUES (1, ?1, 0, 41)",
        [qm],
    )
    .unwrap();
    let task: TaskId = conn
        .query_row(
            "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                 delivery_mode, branch, status, created_at, seq)
             VALUES (100, ?1, ?2, 'build', 't', 'g', 'direct_pr', 'qm/task-0100',
                 'queued', 0, 50) RETURNING id",
            params![project, qm],
            |r| r.get(0),
        )
        .unwrap();
    (conn, task)
}

/// An engine reading: a 5-hour window and a 7-day window.
fn engine(conn: &Connection, account: i64, five_hour: f64, seven_day: f64) {
    let json = format!(
        r#"{{"windows":[
            {{"name":"five_hour","used_percent":{five_hour},"resets_at":"2026-10-23T03:00:00Z"}},
            {{"name":"seven_day","used_percent":{seven_day},"resets_at":"2026-10-28T00:00:00Z"}}
        ],"read_at":"2026-10-23T00:00:00Z"}}"#
    );
    accounts::record_usage(conn, AccountId::new(account), &json, now()).unwrap();
}

/// A quota check's prepaid budget, `used` percent spent.
fn budget(conn: &Connection, account: i64, used: f64) {
    let json = format!(
        r#"{{"pools":[{{"name":"key_budget","used_percent":{used},"span":"balance","stops_account":true}}]}}"#
    );
    accounts::record_quota(conn, AccountId::new(account), &json, now()).unwrap();
}

fn mark(conn: &Connection, account: i64, percent: Option<u8>) {
    accounts::set_reserve_at(conn, AccountId::new(account), percent).unwrap();
}

fn facts(task: TaskId) -> CrewmateFacts {
    CrewmateFacts {
        task,
        agent: None,
        authority: Authority::Turn(StartedBy::User {
            message: MessageId::new(1),
        }),
        kind: Decided::Answer {
            value: TaskKind::Build,
            by: DecidedBy::Declared,
        },
        decider: DecidedBy::Declared,
        builds_on: None,
        needs: Needs::default(),
        quick_verification: false,
        named_model: None,
        preference: None,
        named_tier: None,
        named_effort: None,
        premium: false,
        choice_message: None,
        stronger_retry: None,
        later_step: None,
        prefer_provider_other_than: None,
    }
}

fn route(conn: &Connection, facts: &CrewmateFacts) -> RoutingRecord {
    match route_crewmate(conn, &Config::default(), facts, &LoginHealth::new(), now()).unwrap() {
        RouteOutcome::Routed(r) => r.record,
        other => panic!("expected a routed crewmate, got {other:?}"),
    }
}

fn account(record: &RoutingRecord) -> i64 {
    record.choice.as_ref().unwrap().account.get()
}

#[test]
fn the_worked_example_routes_to_claude() {
    let (conn, task) = fixture();
    // The 5-hour windows are tighter, but the weekly windows are the largest pools.
    engine(&conn, CLAUDE, 95.0, 60.0);
    engine(&conn, CHATGPT, 10.0, 40.0);
    budget(&conn, DEEPSEEK, 80.0);
    mark(&conn, CLAUDE, Some(55));
    mark(&conn, CHATGPT, Some(30));
    mark(&conn, DEEPSEEK, Some(75));
    let record = route(&conn, &facts(task));
    assert_eq!(account(&record), CLAUDE, "{}", record.why);
    assert!(
        record
            .why
            .contains("Quota chose its claude account (60% used, past its 55% soft limit)"),
        "{}",
        record.why
    );
}

#[test]
fn with_no_marks_the_most_quota_left_serves_and_unknown_keys_wait() {
    let (conn, task) = fixture();
    engine(&conn, CLAUDE, 5.0, 60.0);
    engine(&conn, CHATGPT, 50.0, 40.0);
    // DeepSeek has no reading: a pay-per-token key with unknown quota serves last.
    assert_eq!(account(&route(&conn, &facts(task))), CHATGPT);
    // Its budget known and barely touched, it competes and wins.
    budget(&conn, DEEPSEEK, 5.0);
    assert_eq!(account(&route(&conn, &facts(task))), DEEPSEEK);
    // Spent, it is cut off and skipped.
    budget(&conn, DEEPSEEK, 100.0);
    let record = route(&conn, &facts(task));
    assert_eq!(account(&record), CHATGPT);
    assert!(record.skipped.iter().any(|s| s.account.get() == DEEPSEEK));
}

#[test]
fn a_named_model_still_overrides_quota() {
    let (conn, task) = fixture();
    engine(&conn, CLAUDE, 5.0, 95.0);
    mark(&conn, CLAUDE, Some(50));
    engine(&conn, CHATGPT, 5.0, 5.0);
    let mut facts = facts(task);
    facts.named_model = Some(ModelId::new("claude-opus-5-5").unwrap());
    assert_eq!(account(&route(&conn, &facts)), CLAUDE);
}

#[test]
fn a_soft_limit_outside_one_to_ninety_nine_is_refused() {
    let (conn, _) = fixture();
    for bad in [0, 100] {
        assert!(accounts::set_reserve_at(&conn, AccountId::new(CLAUDE), Some(bad)).is_err());
    }
    mark(&conn, CLAUDE, Some(1));
    mark(&conn, CLAUDE, Some(99));
    mark(&conn, CLAUDE, None);
    let row = accounts::load_account(&conn, AccountId::new(CLAUDE))
        .unwrap()
        .unwrap();
    assert_eq!(row.reserve_at, None);
}

/// A DeepSeek balance check, stored the way Settings stores it: measured against the reading
/// already on the row.
fn balance_check(conn: &Connection, account: i64, usd: f64) {
    use qm_auth::quota::{Currency, Money, QuotaBucket, QuotaKind, QuotaSummary};
    let mut bucket = QuotaBucket::new(QuotaKind::Balance);
    bucket.balance = Some(Money {
        amount: usd,
        currency: Currency::Usd,
    });
    let id = AccountId::new(account);
    let before = accounts::load_account(conn, id).unwrap().unwrap().quota;
    let stored =
        crate::quota_pools::from_summary(&QuotaSummary::new(vec![bucket]), before.as_deref());
    let json = serde_json::to_string(&stored).unwrap();
    accounts::record_quota(conn, id, &json, now()).unwrap();
}

#[test]
fn a_deepseek_balance_competes_on_quota_and_its_soft_limit() {
    let (conn, task) = fixture();
    engine(&conn, CLAUDE, 5.0, 60.0);
    engine(&conn, CHATGPT, 10.0, 40.0);
    // $10 at the first check: the reference, 0% used, so it serves first.
    balance_check(&conn, DEEPSEEK, 10.0);
    assert_eq!(account(&route(&conn, &facts(task))), DEEPSEEK);
    // $5 left of $10: 50% used, more than ChatGPT's 40%.
    balance_check(&conn, DEEPSEEK, 5.0);
    let record = route(&conn, &facts(task));
    assert_eq!(account(&record), CHATGPT, "{}", record.why);
    // A top-up to $6 resets the reference: 0% used again.
    balance_check(&conn, DEEPSEEK, 6.0);
    assert_eq!(account(&route(&conn, &facts(task))), DEEPSEEK);
    // $4.50 of $6 is 25% used: past a 20% soft limit, others serve first.
    balance_check(&conn, DEEPSEEK, 4.5);
    mark(&conn, DEEPSEEK, Some(20));
    mark(&conn, CHATGPT, Some(50));
    let record = route(&conn, &facts(task));
    assert_eq!(account(&record), CHATGPT, "{}", record.why);
    // Empty, it is cut off and skipped.
    balance_check(&conn, DEEPSEEK, 0.0);
    let record = route(&conn, &facts(task));
    assert!(record.skipped.iter().any(|s| s.account.get() == DEEPSEEK));
}
