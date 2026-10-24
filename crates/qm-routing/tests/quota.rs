//! Quota-aware routing within a tier: the account with the most quota left on its largest pool
//! serves, accounts past their reserve mark are held back, and your explicit choices still win.
use qm_routing::choose::{Pool, RouteRequest, route};
use qm_routing::decider::{Decided, DecidedBy};
use qm_routing::rank::{Candidate, Needs, Standing, Usage, rank, rank_steady};
use qm_routing::record::{RoutingRecord, UserChoices};
use qm_routing::rules::RuleTable;
use qm_types::config::{KindTiers, TierEfforts};
use qm_types::domain::{AccountKind, Effort, Provider, TaskKind, Tier};
use qm_types::ids::{AccountId, ModelId};

fn id(name: &str) -> ModelId {
    ModelId::new(name).unwrap()
}

/// One High-tier entry on its own account. `used` is percent USED of the largest pool.
fn on(provider: Provider, account: i64, used: Option<f64>, reserve_at: Option<u8>) -> Candidate {
    Candidate {
        provider,
        model: id(&format!("{provider}-model")),
        family: provider.to_string(),
        generation: 1,
        account: AccountId::new(account),
        account_kind: AccountKind::Subscription,
        tier: Tier::High,
        effort_override: Some(Effort::High),
        efforts: Effort::ALL.to_vec(),
        image_input: true,
        context_window: Some(200_000),
        usage: used.map_or(Usage::Unknown, |used_percent| Usage::Known { used_percent }),
        reserve_at,
        cut_off: false,
    }
}

fn request(tiers: &TierEfforts) -> RouteRequest<'_> {
    RouteRequest {
        kind: Decided::Answer {
            value: TaskKind::Build,
            by: DecidedBy::Declared,
        },
        decider: DecidedBy::Declared,
        builds_on: None,
        needs: Needs::default(),
        quick_verification: false,
        user: UserChoices::default(),
        preference: None,
        stronger_retry_target: None,
        later_step: None,
        always_use: None,
        preferred_account: None,
        tiers,
        premium_models: &[],
    }
}

fn run(req: &RouteRequest<'_>, placed: &[Candidate]) -> RoutingRecord {
    route(
        &RuleTable::from_kinds(&KindTiers::default()),
        req,
        Pool {
            placed,
            pinned: &[],
        },
    )
}

fn chosen(record: &RoutingRecord) -> AccountId {
    record.choice.as_ref().expect("a model serves").account
}

// The worked example: Anthropic 60% used (mark 55%), GPT 40% (mark 30%), DeepSeek 80% (mark
// 75%). All three are in reserve; Anthropic and DeepSeek are both 5 points past, GPT 10, and the
// tie goes to the one with less used overall: Anthropic. DeepSeek is a custom endpoint.
#[test]
fn worked_example_picks_the_account_least_past_its_mark() {
    let efforts = TierEfforts::default();
    let placed = [
        on(Provider::ChatGpt, 2, Some(40.0), Some(30)),
        on(Provider::Custom, 3, Some(80.0), Some(75)),
        on(Provider::Claude, 1, Some(60.0), Some(55)),
    ];
    let record = run(&request(&efforts), &placed);
    assert_eq!(chosen(&record), AccountId::new(1), "{}", record.why);
    assert!(
        record.why.contains(
            "Quota chose its claude account (60% used, past its 55% reserve mark) over custom \
             (80% used, past its 75% reserve mark)"
        ),
        "{}",
        record.why
    );
    // The same in any placement order.
    let mut placed = placed;
    placed.reverse();
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(1));
    placed.rotate_left(1);
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(1));
}

#[test]
fn without_marks_the_most_quota_left_serves() {
    let efforts = TierEfforts::default();
    let placed = [
        on(Provider::Claude, 1, Some(60.0), None),
        on(Provider::ChatGpt, 2, Some(40.0), None),
        on(Provider::Custom, 3, Some(80.0), None),
    ];
    let record = run(&request(&efforts), &placed);
    assert_eq!(chosen(&record), AccountId::new(2), "{}", record.why);
    assert!(
        record
            .why
            .contains("Quota chose its chatgpt account (40% used) over claude (60% used)")
    );
}

#[test]
fn accounts_under_their_mark_come_before_any_in_reserve() {
    let efforts = TierEfforts::default();
    // GPT has more left but is past its mark; Claude is under its own and serves.
    let placed = [
        on(Provider::ChatGpt, 2, Some(35.0), Some(30)),
        on(Provider::Claude, 1, Some(70.0), Some(80)),
    ];
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(1));
    // Among several under their marks, plain quota left decides, not distance to the mark.
    let placed = [
        on(Provider::Claude, 1, Some(50.0), Some(55)),
        on(Provider::ChatGpt, 2, Some(45.0), Some(90)),
        on(Provider::Custom, 3, Some(80.0), Some(75)),
    ];
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(2));
}

#[test]
fn reaching_the_mark_exactly_counts_as_in_reserve() {
    let efforts = TierEfforts::default();
    let placed = [
        on(Provider::Claude, 1, Some(29.0), Some(29)),
        on(Provider::ChatGpt, 2, Some(50.0), None),
    ];
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(2));
    assert!(
        Standing::of(
            Usage::Known { used_percent: 29.0 },
            AccountKind::Subscription,
            Some(29)
        )
        .in_reserve()
    );
    assert!(
        !Standing::of(
            Usage::Known {
                used_percent: 28.99
            },
            AccountKind::Subscription,
            Some(29)
        )
        .in_reserve()
    );
}

#[test]
fn unknown_quota_ranks_between_known_open_and_nearly_used_up_or_reserved() {
    let efforts = TierEfforts::default();
    // Known with room beats unknown.
    let placed = [
        on(Provider::Claude, 1, None, None),
        on(Provider::ChatGpt, 2, Some(70.0), None),
    ];
    let record = run(&request(&efforts), &placed);
    assert_eq!(chosen(&record), AccountId::new(2));
    assert!(
        record.why.contains("over claude (quota unknown)"),
        "{}",
        record.why
    );
    // Unknown beats nearly used up and in reserve: unknown is never read as all left, nor as gone.
    let placed = [
        on(Provider::ChatGpt, 2, Some(95.0), None),
        on(Provider::Custom, 3, Some(40.0), Some(30)),
        on(Provider::Claude, 1, None, Some(10)),
    ];
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(1));
    // Nearly used up but under its mark still beats one in reserve.
    let placed = [
        on(Provider::Custom, 3, Some(40.0), Some(30)),
        on(Provider::ChatGpt, 2, Some(95.0), None),
    ];
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(2));
}

#[test]
fn keys_with_known_quota_compete_but_unknown_keys_serve_last() {
    let efforts = TierEfforts::default();
    let mut key = on(Provider::OpenRouter, 4, Some(10.0), None);
    key.account_kind = AccountKind::ApiKey;
    // A key with a known budget competes on quota left.
    let placed = [on(Provider::Claude, 1, Some(50.0), None), key.clone()];
    assert_eq!(chosen(&run(&request(&efforts), &placed)), AccountId::new(4));
    // A pay-per-token key with no reading serves only when no subscription is ranked, even one in
    // reserve.
    key.usage = Usage::Unknown;
    let placed = [key.clone(), on(Provider::Claude, 1, Some(70.0), Some(50))];
    let record = run(&request(&efforts), &placed);
    assert_eq!(chosen(&record), AccountId::new(1));
    assert!(
        record
            .why
            .contains("over openrouter (pay-per-token, quota unknown)"),
        "{}",
        record.why
    );
}

#[test]
fn an_account_with_no_quota_left_is_never_used() {
    let efforts = TierEfforts::default();
    let mut gone = on(Provider::ChatGpt, 2, Some(100.0), None);
    gone.cut_off = true;
    let placed = [gone, on(Provider::Claude, 1, Some(85.0), Some(50))];
    let record = run(&request(&efforts), &placed);
    assert_eq!(chosen(&record), AccountId::new(1));
    assert!(
        record
            .skipped
            .iter()
            .any(|s| s.account == AccountId::new(2))
    );
}

#[test]
fn explicit_choices_still_override_quota() {
    let efforts = TierEfforts::default();
    let placed = [
        on(Provider::Claude, 1, Some(90.0), Some(50)),
        on(Provider::ChatGpt, 2, Some(10.0), None),
    ];
    // A named model.
    let mut req = request(&efforts);
    req.user.model = Some(id("claude-model"));
    assert_eq!(chosen(&run(&req, &placed)), AccountId::new(1));
    // Your preferred account.
    let mut req = request(&efforts);
    req.preferred_account = Some(AccountId::new(1));
    let record = run(&req, &placed);
    assert_eq!(chosen(&record), AccountId::new(1));
    assert!(!record.why.contains("Quota chose"), "{}", record.why);
    // An explicit tier selects that tier's pool; quota only ranks inside it.
    let mut low = on(Provider::Claude, 1, Some(90.0), Some(50));
    low.tier = Tier::Low;
    let placed = [low, on(Provider::ChatGpt, 2, Some(10.0), None)];
    let mut req = request(&efforts);
    req.user.tier = Some(Tier::Low);
    assert_eq!(chosen(&run(&req, &placed)), AccountId::new(1));
}

#[test]
fn the_quartermasters_own_ranking_ignores_quota() {
    let a = on(Provider::Claude, 1, Some(95.0), Some(50));
    let b = on(Provider::ChatGpt, 2, Some(5.0), None);
    let mut crew = vec![&a, &b];
    rank(&mut crew, None);
    assert_eq!(crew[0].account, AccountId::new(2));
    let mut steady = vec![&a, &b];
    rank_steady(&mut steady, None);
    assert_eq!(steady[0].account, AccountId::new(1));
}

#[test]
fn ties_keep_placement_order_and_name_no_quota_reason() {
    let efforts = TierEfforts::default();
    let placed = [
        on(Provider::ChatGpt, 2, Some(40.0), None),
        on(Provider::Claude, 1, Some(40.0), None),
    ];
    let record = run(&request(&efforts), &placed);
    assert_eq!(chosen(&record), AccountId::new(2));
    assert!(!record.why.contains("Quota chose"), "{}", record.why);
}
