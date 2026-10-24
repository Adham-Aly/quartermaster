//! Explicit tiers select ranked placements, not task kinds or reasoning efforts.
use qm_routing::choose::{Pool, RouteRequest, route};
use qm_routing::decider::{Decided, DecidedBy};
use qm_routing::fallback;
use qm_routing::later_steps::AppliedLaterStep;
use qm_routing::rank::{Candidate, Needs, Usage};
use qm_routing::record::{RoutingRecord, UserChoices};
use qm_routing::rules::RuleTable;
use qm_types::config::{AlwaysUse, KindTiers, TierEfforts};
use qm_types::domain::{AccountKind, Effort, KindRow, Provider, TaskKind, Tier};
use qm_types::ids::{AccountId, ModelId, TaskId};
use qm_types::model_preference::ModelPreference;

const TIERS: [Tier; 4] = [Tier::Low, Tier::Medium, Tier::High, Tier::Premium];

fn id(name: &str) -> ModelId {
    ModelId::new(name).unwrap()
}

fn candidate(name: &str, tier: Tier, effort: Effort) -> Candidate {
    Candidate {
        provider: Provider::Anthropic,
        model: id(name),
        family: name.to_owned(),
        generation: 1,
        account: AccountId::new(1),
        account_kind: AccountKind::Subscription,
        tier,
        effort_override: Some(effort),
        efforts: Effort::ALL.to_vec(),
        image_input: true,
        context_window: Some(200_000),
        usage: Usage::Unknown,
        cut_off: false,
    }
}

fn request(tiers: &TierEfforts) -> RouteRequest<'_> {
    RouteRequest {
        kind: Decided::Answer {
            value: TaskKind::Unclear,
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
    // Deliberately customized mappings: the request must win over the user's kind settings too.
    let mut kinds = KindTiers::default();
    kinds.set(KindRow::Lookup, Some(Tier::Low));
    kinds.set(KindRow::Build, Some(Tier::Medium));
    kinds.set(KindRow::Unclear, Some(Tier::High));
    route(
        &RuleTable::from_kinds(&kinds),
        req,
        Pool {
            placed,
            pinned: placed,
        },
    )
}

#[test]
fn explicit_tiers_select_user_placements_independently_of_kind_and_effort() {
    let efforts = TierEfforts::default();
    let placed = TIERS.map(|tier| candidate(&tier.to_string(), tier, Effort::ExtraHigh));
    for tier in TIERS {
        for kind in [TaskKind::Lookup, TaskKind::Build, TaskKind::Unclear] {
            let mut req = request(&efforts);
            req.kind = Decided::Answer {
                value: kind,
                by: DecidedBy::Declared,
            };
            req.user.tier = Some(tier);
            let record = run(&req, &placed);
            assert_eq!(record.tier_landed, tier);
            assert_eq!(record.facts.user.tier, Some(tier));
            assert_eq!(record.facts.user.effort, None);
            let choice = record.choice.unwrap();
            assert_eq!(choice.tier, tier);
            assert_eq!(choice.model, id(&tier.to_string()));
            assert_eq!(choice.effort, Effort::ExtraHigh);
            assert_eq!(record.fallbacks, Vec::new());
        }
    }
}

#[test]
fn effort_only_never_changes_the_landed_tier() {
    let efforts = TierEfforts::default();
    let placed = TIERS.map(|tier| candidate(&tier.to_string(), tier, Effort::Medium));
    for &effort in Effort::ALL {
        let mut req = request(&efforts);
        req.user.effort = Some(effort);
        let record = run(&req, &placed);
        assert_eq!(record.tier_landed, Tier::High);
        assert_eq!(record.facts.user.tier, None);
        let choice = record.choice.unwrap();
        assert_eq!(choice.tier, Tier::High);
        assert_eq!(choice.effort, effort);
    }
}

#[test]
fn model_and_tier_filter_to_matching_placement_without_implying_premium() {
    let efforts = TierEfforts::default();
    // Premium comes first, and the model also appears in the full premium inventory.
    let placed = [
        candidate("shared", Tier::Premium, Effort::Max),
        candidate("shared", Tier::High, Effort::High),
        candidate("shared", Tier::Medium, Effort::Low),
    ];
    let premium_models = [id("shared")];
    for named in [false, true] {
        let mut req = request(&efforts);
        req.user.tier = Some(Tier::Medium);
        req.user.model = named.then(|| id("shared"));
        req.premium_models = &premium_models;
        let record = run(&req, &placed);
        assert_eq!(record.tier_landed, Tier::Medium);
        assert!(!record.facts.user.premium);
        assert_eq!(record.fallbacks, Vec::new());
        let choice = record.choice.unwrap();
        assert_eq!(choice.model, id("shared"));
        assert_eq!(choice.tier, Tier::Medium);
        assert_eq!(choice.effort, Effort::Low);
    }
}

#[test]
fn named_model_in_wrong_tier_waits_without_substitution() {
    let efforts = TierEfforts::default();
    let mut req = request(&efforts);
    req.user.model = Some(id("named"));
    req.user.tier = Some(Tier::Medium);
    let placed = [
        candidate("named", Tier::High, Effort::Max),
        candidate("medium", Tier::Medium, Effort::Low),
    ];
    let record = run(&req, &placed);
    assert_eq!(record.tier_landed, Tier::Medium);
    assert!(record.choice.is_none());
    assert!(
        record
            .why
            .contains("the task waits for it instead of using another model")
    );
}

#[test]
fn explicit_tier_bypasses_pin_and_suppresses_entire_inherited_later_step() {
    let efforts = TierEfforts::default();
    let pin = AlwaysUse {
        model: id("pinned"),
        fallback: Some(id("inherited")),
    };
    let placed = [
        candidate("pinned", Tier::High, Effort::High),
        candidate("inherited", Tier::Premium, Effort::Max),
        candidate("requested", Tier::Medium, Effort::Low),
    ];
    for tier in TIERS {
        let mut placed = placed.clone();
        placed[2].tier = tier;
        let mut req = request(&efforts);
        req.user.tier = Some(tier);
        req.always_use = Some(&pin);
        req.later_step = Some(AppliedLaterStep {
            from: TaskId::new(43).unwrap(),
            message: None,
            model: Some(id("inherited")),
            premium: true,
            effort: Some(Effort::Max),
        });
        // Give the selected placement the preferred account so it wins within high/premium too.
        placed[2].account = AccountId::new(2);
        req.preferred_account = Some(AccountId::new(2));
        let record = run(&req, &placed);
        assert_eq!(record.tier_landed, tier);
        assert_eq!(record.facts.later_step_from, None);
        let choice = record.choice.unwrap();
        assert_eq!(choice.model, id("requested"));
        assert_eq!(choice.effort, Effort::Low);
    }
}

#[test]
fn preference_selects_within_explicit_tier_not_kind_tier() {
    let efforts = TierEfforts::default();
    let mut req = request(&efforts);
    req.user.tier = Some(Tier::Medium);
    req.preference = Some(ModelPreference {
        selection: id("preferred"),
        provider: Some(Provider::Anthropic),
        model: Some(id("preferred")),
        effort: Some(Effort::Low),
    });
    let placed = [
        candidate("ordinary", Tier::Medium, Effort::High),
        candidate("preferred", Tier::High, Effort::Max),
        candidate("preferred", Tier::Medium, Effort::Low),
    ];
    let record = run(&req, &placed);
    assert_eq!(record.tier_landed, Tier::Medium);
    assert!(record.facts.preference_applied);
    let choice = record.choice.unwrap();
    assert_eq!(choice.model, id("preferred"));
    assert_eq!(choice.tier, Tier::Medium);
    assert_eq!(choice.effort, Effort::Low);
}

#[test]
fn explicit_tier_normal_fallback_preserves_selected_combo() {
    let efforts = TierEfforts::default();
    for landed in TIERS {
        for &used in fallback::order(landed).iter().skip(1) {
            for &effort in Effort::ALL {
                let mut req = request(&efforts);
                req.user.tier = Some(landed);
                let record = run(&req, &[candidate("fallback", used, effort)]);
                assert_eq!(record.tier_landed, landed);
                assert_eq!(record.facts.user.tier, Some(landed));
                assert_eq!(record.fallbacks.last().unwrap().to, used);
                let choice = record.choice.unwrap();
                assert_eq!(choice.tier, used);
                assert_eq!(choice.effort, effort);
            }
        }
    }
}

#[test]
fn decider_failure_retains_explicit_tier_request() {
    let efforts = TierEfforts::default();
    let placed = TIERS.map(|tier| candidate(&tier.to_string(), tier, Effort::Low));
    for tier in TIERS {
        let mut req = request(&efforts);
        req.user.tier = Some(tier);
        req.kind = Decided::Undecided {
            reason: "timed out".to_owned(),
        };
        let record = run(&req, &placed);
        assert_eq!(record.facts.kind, TaskKind::Unclear);
        assert_eq!(record.facts.user.tier, Some(tier));
        assert!(record.why.contains("your explicit tier still applies"));
        assert!(!record.why.contains("routes as unclear"));
        assert_eq!(record.tier_landed, tier);
        assert_eq!(record.choice.unwrap().tier, tier);
    }
}
