//! Routing by task kind: the defaults match the table shipped before the Task kinds settings,
//! your setting wins over a default, and everything above the table still comes first.

use super::*;
use crate::rank::Usage;
use crate::record::BuildsOn;
use crate::rules::row_for;
use qm_types::config::KindTiers;
use qm_types::domain::{AccountKind, KindRow};
use qm_types::ids::{ModelId, TaskId};

fn model(tier: Tier) -> ModelId {
    ModelId::new(format!("model-{tier}")).unwrap()
}

/// One model in each of `tiers`, on its own account.
fn pool(tiers: &[Tier]) -> Vec<Candidate> {
    tiers
        .iter()
        .zip(1..)
        .map(|(&tier, n)| Candidate {
            provider: qm_types::domain::Provider::Anthropic,
            model: model(tier),
            family: format!("family-{tier}"),
            generation: 1,
            account: AccountId::new(n),
            account_kind: AccountKind::Subscription,
            tier,
            effort_override: None,
            efforts: Effort::ALL.to_vec(),
            image_input: true,
            context_window: None,
            usage: Usage::Unknown,
            cut_off: false,
        })
        .collect()
}

const EVERY_TIER: [Tier; 4] = [Tier::Premium, Tier::High, Tier::Medium, Tier::Low];

fn task(n: u32) -> TaskId {
    TaskId::new(n).unwrap()
}

fn request(kind: TaskKind, fact: BuildsOnFact, tiers: &TierEfforts) -> RouteRequest<'_> {
    RouteRequest {
        kind: Decided::Answer {
            value: kind,
            by: DecidedBy::Declared,
        },
        decider: DecidedBy::Declared,
        builds_on: (fact != BuildsOnFact::Nothing).then(|| BuildsOn {
            task: task(244),
            fact,
        }),
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

fn run(kinds: &KindTiers, req: &RouteRequest<'_>, placed: &[Candidate]) -> RoutingRecord {
    route(
        &RuleTable::from_kinds(kinds),
        req,
        Pool {
            placed,
            pinned: &[],
        },
    )
}

/// The table shipped before this setting (`routing.toml`: lookup → low, a build on a finished
/// plan or a fix on a confirmed diagnosis → medium, `*` → high; first match wins), and its why line.
fn shipped(kind: TaskKind, fact: BuildsOnFact) -> (Tier, String) {
    let tier = match (kind, fact) {
        (TaskKind::Lookup, _) => Tier::Low,
        (TaskKind::Build, BuildsOnFact::FinishedPlan)
        | (TaskKind::Fix, BuildsOnFact::ConfirmedDiagnosis) => Tier::Medium,
        _ => Tier::High,
    };
    let why = match (fact, tier) {
        (BuildsOnFact::FinishedPlan, Tier::Medium) => {
            "Building from the plan in task T-0244".into()
        }
        (BuildsOnFact::ConfirmedDiagnosis, Tier::Medium) => {
            "Building on the confirmed diagnosis in task T-0244".into()
        }
        _ => match kind {
            TaskKind::Lookup => "Lookups run on low".into(),
            TaskKind::Unclear => "Unclear work always routes high".into(),
            _ => format!("{kind} work routes {tier}"),
        },
    };
    (tier, why)
}

#[test]
fn unavailable_named_models_wait_instead_of_using_the_pin_or_another_tier() {
    let tiers = TierEfforts::default();
    let pin = AlwaysUse {
        model: model(Tier::Medium),
        fallback: Some(model(Tier::Low)),
    };
    for from_later in [false, true] {
        for tier in EVERY_TIER {
            let mut req = request(TaskKind::Review, BuildsOnFact::Nothing, &tiers);
            if from_later {
                req.later_step = Some(AppliedLaterStep {
                    from: task(7),
                    message: None,
                    model: Some(model(tier)),
                    effort: Some(Effort::High),
                    premium: false,
                });
            } else {
                req.user.model = Some(model(tier));
                req.user.effort = Some(Effort::High);
            }
            req.always_use = Some(&pin);
            let pinned = pool(&[Tier::Medium, Tier::Low]);
            for missing in [false, true] {
                let mut placed = pool(&EVERY_TIER);
                if missing {
                    placed.retain(|c| c.model != model(tier));
                } else {
                    placed
                        .iter_mut()
                        .find(|c| c.model == model(tier))
                        .unwrap()
                        .cut_off = true;
                }
                let record = route(
                    &RuleTable::from_kinds(&KindTiers::default()),
                    &req,
                    Pool {
                        placed: &placed,
                        pinned: &pinned,
                    },
                );
                assert!(record.choice.is_none(), "{}", record.why);
                assert_eq!(record.fallbacks, []);
                assert!(record.why.contains(model(tier).as_str()), "{}", record.why);
                assert!(record.why.contains("wait"), "{}", record.why);
                if !missing {
                    assert!(record.why.contains("at its limit"), "{}", record.why);
                }
                assert_eq!(record.facts.later_step_from, from_later.then(|| task(7)));
            }
        }
    }
}

#[test]
fn the_defaults_route_as_the_shipped_table_did() {
    let tiers = TierEfforts::default();
    let placed = pool(&EVERY_TIER);
    for &kind in TaskKind::ALL {
        for fact in BuildsOnFact::ALL {
            let (tier, why) = shipped(kind, fact);
            let record = run(&KindTiers::default(), &request(kind, fact, &tiers), &placed);
            assert_eq!(record.tier_landed, tier, "{kind} on {fact:?}");
            assert_eq!(record.choice.unwrap().tier, tier, "{kind} on {fact:?}");
            assert_eq!(record.why, why, "{kind} on {fact:?}");
            assert!(
                record.rule.ends_with(&format!("→ {tier} (default)")),
                "{}",
                record.rule
            );
        }
    }
}

#[test]
fn the_record_names_the_row_and_where_its_tier_came_from() {
    let tiers = TierEfforts::default();
    let placed = pool(&EVERY_TIER);
    let record = run(
        &KindTiers::default(),
        &request(TaskKind::Fix, BuildsOnFact::ConfirmedDiagnosis, &tiers),
        &placed,
    );
    assert_eq!(
        record.rule,
        "kind = \"fix\", builds_on = \"confirmed_diagnosis\" → medium (default)"
    );
    let mut kinds = KindTiers::default();
    kinds.set(KindRow::Review, Some(Tier::Premium));
    let record = run(
        &kinds,
        &request(TaskKind::Review, BuildsOnFact::Nothing, &tiers),
        &placed,
    );
    assert_eq!(record.rule, "kind = \"review\" → premium (your setting)");
    assert_eq!(record.why, "You set review work to route premium");
}

#[test]
fn your_setting_wins_for_every_row_and_tier() {
    let tiers = TierEfforts::default();
    let placed = pool(&EVERY_TIER);
    for &row in KindRow::ALL {
        let fact = match row {
            KindRow::BuildOnFinishedPlan => BuildsOnFact::FinishedPlan,
            KindRow::FixOnConfirmedDiagnosis => BuildsOnFact::ConfirmedDiagnosis,
            _ => BuildsOnFact::Nothing,
        };
        assert_eq!(row_for(row.kind(), fact), row);
        for tier in EVERY_TIER {
            let mut kinds = KindTiers::default();
            kinds.set(row, Some(tier));
            let record = run(&kinds, &request(row.kind(), fact, &tiers), &placed);
            assert_eq!(record.tier_landed, tier, "{row} at {tier}");
            assert_eq!(record.choice.unwrap().model, model(tier), "{row} at {tier}");
        }
    }
}

#[test]
fn a_builds_on_row_may_be_stronger_than_its_kind() {
    let tiers = TierEfforts::default();
    let placed = pool(&EVERY_TIER);
    let mut kinds = KindTiers::default();
    kinds.set(KindRow::Build, Some(Tier::Low));
    kinds.set(KindRow::BuildOnFinishedPlan, Some(Tier::High));
    let on_plan = run(
        &kinds,
        &request(TaskKind::Build, BuildsOnFact::FinishedPlan, &tiers),
        &placed,
    );
    assert_eq!(on_plan.tier_landed, Tier::High);
    assert_eq!(on_plan.why, "Building from the plan in task T-0244");
    let plain = run(
        &kinds,
        &request(TaskKind::Build, BuildsOnFact::Nothing, &tiers),
        &placed,
    );
    assert_eq!(plain.tier_landed, Tier::Low);
    // A build on a confirmed diagnosis isn't a fix: it routes by the build row.
    let on_diagnosis = run(
        &kinds,
        &request(TaskKind::Build, BuildsOnFact::ConfirmedDiagnosis, &tiers),
        &placed,
    );
    assert_eq!(on_diagnosis.tier_landed, Tier::Low);
}

#[test]
fn a_kind_set_to_premium_falls_back_high_then_medium_then_low() {
    let tiers = TierEfforts::default();
    let mut kinds = KindTiers::default();
    kinds.set(KindRow::Review, Some(Tier::Premium));
    let req = request(TaskKind::Review, BuildsOnFact::Nothing, &tiers);
    for (placed, used, steps) in [
        (vec![Tier::High, Tier::Low], Tier::High, 1),
        (vec![Tier::Medium, Tier::Low], Tier::Medium, 2),
        (vec![Tier::Low], Tier::Low, 3),
    ] {
        let record = run(&kinds, &req, &pool(&placed));
        let choice = record.choice.clone().unwrap();
        assert_eq!(record.tier_landed, Tier::Premium);
        assert_eq!(choice.tier, used);
        assert_eq!(record.fallbacks.len(), steps, "{:?}", record.fallbacks);
        assert_eq!(record.fallbacks[0].from, Tier::Premium);
        assert_eq!(record.fallbacks.last().unwrap().to, used);
        assert_eq!(
            record.why,
            format!(
                "You set review work to route premium. No model is placed in premium, so premium \
                 runs on {used}"
            )
        );
        // It keeps premium's effort.
        assert_eq!(choice.effort, tiers.premium);
    }
}

#[test]
fn premium_you_ask_for_falls_back_the_same_way() {
    let tiers = TierEfforts::default();
    let mut req = request(TaskKind::Build, BuildsOnFact::Nothing, &tiers);
    req.user.premium = true;
    let record = run(&KindTiers::default(), &req, &pool(&[Tier::High, Tier::Low]));
    assert_eq!(record.choice.unwrap().tier, Tier::High);
    assert_eq!(
        record.why,
        "You asked for premium. No model is placed in premium, so premium runs on high"
    );
    // With no model at all it waits.
    let record = run(&KindTiers::default(), &req, &[]);
    assert!(record.choice.is_none());
    assert_eq!(
        record.why,
        "No model in any tier can serve this task; it waits"
    );
}

#[test]
fn a_stronger_retry_to_premium_falls_back_but_one_to_high_waits() {
    let tiers = TierEfforts::default();
    let mut req = request(TaskKind::Build, BuildsOnFact::Nothing, &tiers);
    req.user.stronger_retry = true;
    req.stronger_retry_target = Some(Tier::Premium);
    let record = run(&KindTiers::default(), &req, &pool(&[Tier::High, Tier::Low]));
    assert_eq!(record.choice.unwrap().tier, Tier::High);
    assert_eq!(
        record.why,
        "You chose to try a stronger model. No model is placed in premium, so premium runs on high"
    );
    let record = run(&KindTiers::default(), &req, &[]);
    assert!(record.choice.is_none());
    assert_eq!(
        record.why,
        "No model in any tier can serve this task; it waits"
    );

    req.stronger_retry_target = Some(Tier::High);
    let record = run(
        &KindTiers::default(),
        &req,
        &pool(&[Tier::Medium, Tier::Low]),
    );
    assert!(record.choice.is_none());
    assert_eq!(record.why, "waiting for a high model");
}

#[test]
fn an_undecided_kind_routes_by_the_unclear_row() {
    let tiers = TierEfforts::default();
    let placed = pool(&EVERY_TIER);
    let mut req = request(TaskKind::Build, BuildsOnFact::Nothing, &tiers);
    req.kind = Decided::Undecided {
        reason: "timed out".into(),
    };
    let record = run(&KindTiers::default(), &req, &placed);
    assert_eq!(record.tier_landed, Tier::High);
    assert!(
        record
            .why
            .ends_with("The kind couldn't be decided, so it routes as unclear (high)"),
        "{}",
        record.why
    );
    let mut kinds = KindTiers::default();
    kinds.set(KindRow::Unclear, Some(Tier::Low));
    let record = run(&kinds, &req, &placed);
    assert_eq!(record.tier_landed, Tier::Low);
    assert!(
        record.why.ends_with("so it routes as unclear (low)"),
        "{}",
        record.why
    );
}

#[test]
fn everything_above_the_table_still_beats_a_changed_row() {
    let tiers = TierEfforts::default();
    let placed = pool(&EVERY_TIER);
    let mut kinds = KindTiers::default();
    kinds.set(KindRow::Review, Some(Tier::Low));
    let base = request(TaskKind::Review, BuildsOnFact::Nothing, &tiers);

    let mut named = base.clone();
    named.user.model = Some(model(Tier::Premium));
    assert_eq!(
        run(&kinds, &named, &placed).choice.unwrap().model,
        model(Tier::Premium)
    );

    let mut later = base.clone();
    later.later_step = Some(AppliedLaterStep {
        from: task(7),
        message: None,
        model: Some(model(Tier::High)),
        effort: None,
        premium: false,
    });
    assert_eq!(
        run(&kinds, &later, &placed).choice.unwrap().model,
        model(Tier::High)
    );

    let mut later_premium = base.clone();
    later_premium.later_step = Some(AppliedLaterStep {
        from: task(7),
        message: None,
        model: None,
        effort: None,
        premium: true,
    });
    assert_eq!(
        run(&kinds, &later_premium, &placed).choice.unwrap().tier,
        Tier::Premium
    );

    let mut premium = base.clone();
    premium.user.premium = true;
    assert_eq!(
        run(&kinds, &premium, &placed).choice.unwrap().tier,
        Tier::Premium
    );

    let mut retry = base.clone();
    retry.user.stronger_retry = true;
    retry.stronger_retry_target = Some(Tier::Medium);
    assert_eq!(
        run(&kinds, &retry, &placed).choice.unwrap().tier,
        Tier::Medium
    );

    let pin = AlwaysUse {
        model: model(Tier::High),
        fallback: None,
    };
    let mut pinned = base;
    pinned.always_use = Some(&pin);
    let pinned_pool = pool(&[Tier::High]);
    let record = route(
        &RuleTable::from_kinds(&kinds),
        &pinned,
        Pool {
            placed: &placed,
            pinned: &pinned_pool,
        },
    );
    assert_eq!(record.choice.unwrap().model, model(Tier::High));
    assert_eq!(record.why, "You set this model to always be used");
}
