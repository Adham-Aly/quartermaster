//! A preference is a revalidated within-tier decision, not a model override.
use qm_routing::choose::{Pool, RouteRequest, route};
use qm_routing::decider::{Decided, DecidedBy};
use qm_routing::later_steps::AppliedLaterStep;
use qm_routing::rank::{Candidate, Needs, Usage};
use qm_routing::record::{RoutingRecord, UserChoices};
use qm_routing::rules::RuleTable;
use qm_types::config::{AlwaysUse, KindTiers, TierEfforts};
use qm_types::domain::{AccountKind, Effort, Provider, TaskKind, Tier};
use qm_types::ids::{AccountId, ModelId, TaskId};
use qm_types::model_preference::ModelPreference;

fn id(name: &str) -> ModelId {
    ModelId::new(name).unwrap()
}

fn candidate(name: &str, account: i64, tier: Tier) -> Candidate {
    Candidate {
        provider: Provider::Anthropic,
        model: id(name),
        family: name.to_owned(),
        generation: 1,
        account: AccountId::new(account),
        account_kind: AccountKind::Subscription,
        tier,
        effort_override: Some(Effort::High),
        efforts: Effort::ALL.to_vec(),
        image_input: true,
        context_window: Some(200_000),
        usage: Usage::Unknown,
        reserve_at: None,
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
        preference: Some(ModelPreference {
            selection: id("strongest"),
            provider: None,
            model: None,
            effort: None,
        }),
        stronger_retry_target: None,
        later_step: None,
        always_use: None,
        preferred_account: Some(AccountId::new(1)),
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
            pinned: placed,
        },
    )
}

fn pool() -> Vec<Candidate> {
    vec![
        candidate("ordinary", 1, Tier::High),
        candidate("strongest", 2, Tier::High),
        candidate("premium", 3, Tier::Premium),
    ]
}

fn assert_fallback(record: &RoutingRecord) {
    let choice = record.choice.as_ref().unwrap();
    assert_eq!(choice.model, id("ordinary"));
    assert_eq!(choice.tier, Tier::High);
    assert!(
        record
            .why
            .contains("couldn't meet every requirement in high"),
        "{}",
        record.why
    );
    assert!(record.why.contains("normal routing was used"));
}

#[test]
fn qm_selected_strongest_beats_account_preference_and_is_recorded() {
    let tiers = TierEfforts::default();
    let req = request(&tiers);
    let record = run(&req, &pool());
    let choice = record.choice.unwrap();
    assert_eq!(choice.model, id("strongest"));
    assert_eq!(choice.account, AccountId::new(2));
    assert_eq!(choice.effort, Effort::High);
    assert_eq!(record.facts.preference, req.preference);
    assert!(
        record
            .why
            .contains("Your model preference selected strongest within high")
    );
    assert_eq!(record.fallbacks, Vec::new());
}

#[test]
fn provider_model_and_effort_requirements_are_exact_and_conjunctive() {
    let tiers = TierEfforts::default();
    for mismatch in 0..4 {
        let mut req = request(&tiers);
        let preference = req.preference.as_mut().unwrap();
        preference.provider = Some(Provider::Anthropic);
        preference.model = Some(id("strongest"));
        preference.effort = Some(Effort::High);
        match mismatch {
            1 => preference.provider = Some(Provider::OpenAi),
            2 => preference.model = Some(id("ordinary")),
            3 => preference.effort = Some(Effort::Max),
            _ => {}
        }
        let record = run(&req, &pool());
        if mismatch == 0 {
            assert_eq!(record.choice.unwrap().model, id("strongest"));
        } else {
            assert_fallback(&record);
        }
    }
}

#[test]
fn stale_or_out_of_tier_selection_falls_back_without_asking_for_premium() {
    let tiers = TierEfforts::default();
    for selection in ["missing", "premium", "low"] {
        let mut req = request(&tiers);
        req.preference.as_mut().unwrap().selection = id(selection);
        let mut placed = pool();
        placed.push(candidate("low", 4, Tier::Low));
        let record = run(&req, &placed);
        assert_fallback(&record);
        assert_eq!(record.tier_landed, Tier::High);
        assert!(!record.facts.user.premium);
    }
}

#[test]
fn preference_cannot_escape_to_premium_when_regular_tiers_are_empty() {
    let tiers = TierEfforts::default();
    let mut req = request(&tiers);
    req.preference.as_mut().unwrap().selection = id("premium");
    let record = run(&req, &[candidate("premium", 3, Tier::Premium)]);
    assert!(record.choice.is_none());
    assert_eq!(record.tier_landed, Tier::High);
    assert!(record.why.contains("normal routing was used"));
    assert!(record.fallbacks.iter().all(|step| step.to != Tier::Premium));
}

#[test]
fn capability_cutoff_and_unsupported_combo_are_revalidated() {
    let tiers = TierEfforts::default();
    for failure in 0..4 {
        let mut req = request(&tiers);
        let mut placed = pool();
        match failure {
            0 => {
                req.needs.image_input = true;
                placed[1].image_input = false;
            }
            1 => {
                req.needs.context_tokens = Some(100_000);
                placed[1].context_window = Some(1_000);
            }
            2 => placed[1].cut_off = true,
            _ => placed[1].efforts = vec![Effort::Low],
        }
        let record = run(&req, &placed);
        assert_fallback(&record);
        assert!(
            record
                .skipped
                .iter()
                .any(|skipped| skipped.model == id("strongest"))
        );
    }
}

#[test]
fn effort_preference_never_invents_a_combo_even_when_model_supports_it() {
    let tiers = TierEfforts::default();
    let mut req = request(&tiers);
    req.preference.as_mut().unwrap().effort = Some(Effort::Max);
    let mut placed = pool();
    assert_fallback(&run(&req, &placed));
    placed.push(Candidate {
        effort_override: Some(Effort::Max),
        ..candidate("strongest", 2, Tier::High)
    });
    let choice = run(&req, &placed).choice.unwrap();
    assert_eq!(choice.model, id("strongest"));
    assert_eq!(choice.effort, Effort::Max);
}

#[test]
fn no_effort_candidate_cannot_satisfy_a_requested_tier_default() {
    let tiers = TierEfforts::default();
    let mut req = request(&tiers);
    req.preference.as_mut().unwrap().effort = Some(tiers.high);
    let mut placed = pool();
    placed[1].efforts.clear();
    placed[1].effort_override = None;
    let record = run(&req, &placed);
    assert_fallback(&record);
    assert!(!record.facts.preference_applied);
}

#[test]
fn explicit_model_effort_pin_premium_retry_and_later_step_take_priority() {
    let tiers = TierEfforts::default();
    let pin = AlwaysUse {
        model: id("ordinary"),
        fallback: None,
    };
    for priority in 0..6 {
        let mut req = request(&tiers);
        let expected = match priority {
            0 => {
                req.user.model = Some(id("ordinary"));
                "ordinary"
            }
            1 => {
                req.user.effort = Some(Effort::Low);
                "ordinary"
            }
            2 => {
                req.always_use = Some(&pin);
                "ordinary"
            }
            3 => {
                req.user.premium = true;
                "premium"
            }
            4 => {
                req.user.stronger_retry = true;
                req.stronger_retry_target = Some(Tier::Premium);
                "premium"
            }
            _ => {
                req.later_step = Some(AppliedLaterStep {
                    from: TaskId::new(1).unwrap(),
                    message: None,
                    model: Some(id("ordinary")),
                    effort: None,
                    premium: false,
                });
                "ordinary"
            }
        };
        let record = run(&req, &pool());
        assert_eq!(record.choice.unwrap().model, id(expected));
        assert!(!record.why.contains("Your model preference selected"));
        assert!(!record.why.contains("normal routing was used"));
    }
}
