//! Fixed tier-entry combos, independent of the shipped task-kind mapping.
use qm_routing::choose::{Pool, RouteRequest, route};
use qm_routing::decider::{Decided, DecidedBy};
use qm_routing::effort::{EffortInput, choose_effort, tier_default};
use qm_routing::fallback;
use qm_routing::later_steps::AppliedLaterStep;
use qm_routing::quartermaster_model::{self, SeatChoice};
use qm_routing::rank::{Candidate, Needs, Usage};
use qm_routing::record::{FallbackStep, RoutingRecord, UserChoices};
use qm_routing::rules::RuleTable;
use qm_types::config::{AlwaysUse, QuartermasterModel, TierEfforts};
use qm_types::domain::{AccountKind, Effort, TaskKind, Tier};
use qm_types::ids::{AccountId, ModelId, TaskId};

const EFFORTS: [Effort; 5] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::ExtraHigh,
    Effort::Max,
];

fn candidate(tier: Tier, effort: Option<Effort>) -> Candidate {
    Candidate {
        model: ModelId::new("same-model").unwrap(),
        family: "same-family".to_owned(),
        generation: 1,
        account: AccountId::new(1),
        account_kind: AccountKind::Subscription,
        tier,
        effort_override: effort,
        efforts: EFFORTS.to_vec(),
        image_input: true,
        context_window: Some(200_000),
        usage: Usage::Unknown,
        cut_off: false,
    }
}

fn request(tiers: &TierEfforts, landed: Tier) -> RouteRequest<'_> {
    RouteRequest {
        kind: Decided::Answer {
            value: match landed {
                Tier::Low => TaskKind::Lookup,
                Tier::Medium => TaskKind::Build,
                _ => TaskKind::Unclear,
            },
            by: DecidedBy::Declared,
        },
        decider: DecidedBy::Declared,
        builds_on: None,
        needs: Needs::default(),
        quick_verification: false,
        user: UserChoices {
            premium: landed == Tier::Premium,
            ..UserChoices::default()
        },
        stronger_retry_target: None,
        later_step: None,
        always_use: None,
        preferred_account: None,
        tiers,
        premium_models: &[],
    }
}

fn run(req: &RouteRequest<'_>, placed: &[Candidate], pinned: &[Candidate]) -> RoutingRecord {
    // Test effort selection, not the separately maintained task-kind mapping.
    let table = RuleTable::parse(
        r#"rules = [
            { kind = "lookup", tier = "low" },
            { kind = "build", tier = "medium" },
            { kind = "*", tier = "high" },
        ]"#,
    )
    .unwrap();
    route(&table, req, Pool { placed, pinned })
}

fn seat(
    setting: &QuartermasterModel,
    effort: Option<Effort>,
    placed: &[Candidate],
    preferred: Option<AccountId>,
) -> Option<SeatChoice> {
    quartermaster_model::choose(setting, effort, placed, preferred, &TierEfforts::default())
}

#[test]
fn same_model_in_distinct_tiers_runs_each_entries_effort() {
    let tiers = TierEfforts::default();
    let placed = [
        candidate(Tier::High, Some(Effort::Max)),
        candidate(Tier::Medium, Some(Effort::High)),
        candidate(Tier::Low, Some(Effort::Low)),
    ];
    for c in &placed {
        let record = run(&request(&tiers, c.tier), &placed, &[]);
        let choice = record.choice.unwrap();
        assert_eq!(choice.model, c.model);
        assert_eq!(choice.effort, c.effort_override.unwrap());
        assert_eq!(choice.tier, c.tier);
        assert_eq!(record.fallbacks, Vec::<FallbackStep>::new());
    }
}

#[test]
fn same_model_distinct_efforts_in_one_tier_retain_entry_order() {
    let tiers = TierEfforts::default();
    for efforts in [[Effort::High, Effort::Low], [Effort::Low, Effort::High]] {
        let placed = efforts.map(|e| candidate(Tier::High, Some(e)));
        let choice = run(&request(&tiers, Tier::High), &placed, &[])
            .choice
            .unwrap();
        assert_eq!(choice.effort, efforts[0]);
    }
}

#[test]
fn every_fallback_runs_the_selected_combo_not_the_landed_default() {
    let tiers = TierEfforts::default();
    for landed in [Tier::Premium, Tier::High, Tier::Medium, Tier::Low] {
        for &used in fallback::order(landed).iter().skip(1) {
            for effort in EFFORTS {
                let placed = [candidate(used, Some(effort))];
                let record = run(&request(&tiers, landed), &placed, &[]);
                assert_eq!(record.tier_landed, landed);
                let choice = record.choice.unwrap();
                assert_eq!(choice.tier, used);
                assert_eq!(choice.effort, effort, "{landed} -> {used}");
                assert_eq!(record.fallbacks.last().unwrap().to, used);
                assert!(record.why.contains(&fallback::runs_on_line(landed, used)));
            }
        }
    }
}

#[test]
fn unsupported_preferred_subscription_cannot_cap_a_combo() {
    let tiers = TierEfforts::default();
    // Include holes, a lower ceiling, a higher floor and no advertised effort support.
    for unsupported in [
        vec![Effort::Low, Effort::Max],
        vec![Effort::Low],
        vec![Effort::Max],
        vec![],
    ] {
        for used in [Tier::High, Tier::Medium] {
            let mut wrong = candidate(used, Some(Effort::High));
            wrong.efforts = unsupported.clone();
            let mut right = candidate(used, Some(Effort::High));
            right.account = AccountId::new(2);
            right.account_kind = AccountKind::ApiKey;
            right.efforts = vec![Effort::High];
            let mut req = request(&tiers, Tier::High);
            req.preferred_account = Some(wrong.account);
            let record = run(&req, &[wrong, right.clone()], &[]);
            let choice = record.choice.unwrap();
            assert_eq!(choice.account, right.account);
            assert_eq!(choice.effort, Effort::High);
            assert_eq!(record.skipped.len(), 1);
            assert!(
                record.skipped[0]
                    .reason
                    .contains("selected entry's high effort")
            );
        }
    }
}

#[test]
fn unsupported_combo_falls_back_to_another_combo_or_waits() {
    let tiers = TierEfforts::default();
    let mut wrong = candidate(Tier::High, Some(Effort::Max));
    wrong.efforts = vec![Effort::Low];
    let right = candidate(Tier::Medium, Some(Effort::Medium));
    let req = request(&tiers, Tier::High);
    let record = run(&req, &[wrong.clone(), right], &[]);
    assert_eq!(record.choice.unwrap().effort, Effort::Medium);
    assert_eq!(
        record.fallbacks,
        vec![FallbackStep {
            from: Tier::High,
            to: Tier::Medium
        }]
    );
    assert!(run(&req, &[wrong], &[]).choice.is_none());
}

#[test]
fn unsupported_entry_does_not_hide_supported_entry_for_same_model_and_account() {
    let tiers = TierEfforts::default();
    let mut wrong = candidate(Tier::High, Some(Effort::Max));
    wrong.efforts = vec![Effort::High];
    let mut right = wrong.clone();
    right.effort_override = Some(Effort::High);
    let record = run(&request(&tiers, Tier::High), &[wrong, right], &[]);
    assert_eq!(record.choice.unwrap().effort, Effort::High);
    assert_eq!(record.skipped.len(), 1);
}

#[test]
fn named_model_still_uses_its_selected_entry_combo() {
    let tiers = TierEfforts::default();
    let mut wrong = candidate(Tier::Medium, Some(Effort::Max));
    wrong.efforts = vec![Effort::Low];
    let mut right = wrong.clone();
    right.account = AccountId::new(2);
    right.efforts = vec![Effort::Max];
    let mut req = request(&tiers, Tier::High);
    req.user.model = Some(right.model.clone());
    let record = run(&req, &[wrong, right.clone()], &[]);
    let choice = record.choice.unwrap();
    assert_eq!(choice.account, right.account);
    assert_eq!(choice.effort, Effort::Max);
    assert!(record.why.contains("You asked for this model"));
}

#[test]
fn task_and_later_step_efforts_override_fallback_combo_and_keep_capping() {
    let tiers = TierEfforts::default();
    // The combo itself is unsupported, but your explicit task effort deliberately replaces it.
    let mut c = candidate(Tier::Medium, Some(Effort::Max));
    c.efforts = vec![Effort::Low, Effort::High];
    let mut req = request(&tiers, Tier::High);
    req.quick_verification = true;
    req.later_step = Some(AppliedLaterStep {
        from: TaskId::new(43).unwrap(),
        message: None,
        model: None,
        premium: false,
        effort: Some(Effort::ExtraHigh),
    });
    let record = run(&req, std::slice::from_ref(&c), &[]);
    assert_eq!(record.choice.unwrap().effort, Effort::High);
    assert_eq!(record.facts.later_step_from, Some(TaskId::new(43).unwrap()));
    req.user.effort = Some(Effort::Low);
    assert_eq!(run(&req, &[c], &[]).choice.unwrap().effort, Effort::Low);
}

#[test]
fn quick_verification_does_not_step_down_explicit_combo_even_on_fallback() {
    let tiers = TierEfforts::default();
    let mut req = request(&tiers, Tier::High);
    req.quick_verification = true;
    for used in [Tier::High, Tier::Medium] {
        let placed = [candidate(used, Some(Effort::High))];
        assert_eq!(run(&req, &placed, &[]).choice.unwrap().effort, Effort::High);
    }
}

#[test]
fn legacy_entries_keep_landed_default_quick_step_and_caps() {
    let tiers = TierEfforts::default();
    let mut req = request(&tiers, Tier::High);
    let mut c = candidate(Tier::Medium, None);
    assert_eq!(
        run(&req, std::slice::from_ref(&c), &[])
            .choice
            .unwrap()
            .effort,
        tiers.high
    );
    req.quick_verification = true;
    assert_eq!(
        run(&req, std::slice::from_ref(&c), &[])
            .choice
            .unwrap()
            .effort,
        tiers.high.step_down()
    );
    c.efforts = vec![Effort::Low];
    assert_eq!(
        run(&req, std::slice::from_ref(&c), &[])
            .choice
            .unwrap()
            .effort,
        Effort::Low
    );
    c.efforts.clear();
    assert_eq!(
        run(&req, &[c], &[]).choice.unwrap().effort,
        tiers.high.step_down()
    );
}

#[test]
fn stronger_retry_uses_target_combo_and_ignores_carried_effort() {
    let tiers = TierEfforts::default();
    let mut req = request(&tiers, Tier::Low);
    req.user.stronger_retry = true;
    req.stronger_retry_target = Some(Tier::High);
    req.later_step = Some(AppliedLaterStep {
        from: TaskId::new(43).unwrap(),
        message: None,
        model: None,
        premium: false,
        effort: Some(Effort::Low),
    });
    let placed = [candidate(Tier::High, Some(Effort::Max))];
    let record = run(&req, &placed, &[]);
    assert_eq!(record.choice.unwrap().effort, Effort::Max);
    assert_eq!(record.facts.later_step_from, None);
    assert_eq!(record.why, "You chose to try a stronger model");
    req.user.effort = Some(Effort::Medium);
    assert_eq!(
        run(&req, &placed, &[]).choice.unwrap().effort,
        Effort::Medium
    );
    assert!(
        run(&req, &[candidate(Tier::Medium, Some(Effort::High))], &[])
            .choice
            .is_none()
    );
}

#[test]
fn premium_request_runs_its_selected_combo() {
    let tiers = TierEfforts::default();
    let premium = candidate(Tier::Premium, Some(Effort::Max));
    let req = request(&tiers, Tier::Premium);
    let record = run(&req, &[premium], &[]);
    assert_eq!(record.choice.unwrap().effort, Effort::Max);
    assert_eq!(record.why, "You asked for premium");
}

#[test]
fn pins_and_pin_fallbacks_keep_model_only_effort_rules() {
    let tiers = TierEfforts::default();
    let mut c = candidate(Tier::High, Some(Effort::Max));
    c.efforts = vec![Effort::Low, Effort::High];
    for fallback in [false, true] {
        let pin = AlwaysUse {
            model: if fallback {
                ModelId::new("unavailable").unwrap()
            } else {
                c.model.clone()
            },
            fallback: fallback.then(|| c.model.clone()),
        };
        let mut req = request(&tiers, Tier::High);
        req.always_use = Some(&pin);
        assert_eq!(
            run(&req, &[], std::slice::from_ref(&c))
                .choice
                .unwrap()
                .effort,
            Effort::High
        );
        req.user.effort = Some(Effort::Low);
        assert_eq!(
            run(&req, &[], std::slice::from_ref(&c))
                .choice
                .unwrap()
                .effort,
            Effort::Low
        );
    }
}

#[test]
fn auto_uses_selected_combo_including_on_its_tier_fallback() {
    for used in quartermaster_model::AUTO_ORDER {
        let placed = [candidate(used, Some(Effort::Low))];
        let choice = seat(&QuartermasterModel::Auto, None, &placed, None).unwrap();
        assert_eq!(choice.tier, Some(used));
        assert_eq!(choice.effort, Effort::Low);
    }
}

#[test]
fn auto_filters_unsupported_accounts_and_preserves_subscription_only_policy() {
    let mut wrong = candidate(Tier::High, Some(Effort::Max));
    wrong.efforts = vec![Effort::High];
    let mut right = wrong.clone();
    right.account = AccountId::new(2);
    right.efforts = vec![Effort::Max];
    let preferred = Some(wrong.account);
    let placed = [wrong.clone(), right.clone()];
    let choice = seat(&QuartermasterModel::Auto, None, &placed, preferred).unwrap();
    assert_eq!(choice.account, right.account);
    assert_eq!(choice.effort, Effort::Max);
    right.account_kind = AccountKind::ApiKey;
    assert!(
        seat(
            &QuartermasterModel::Auto,
            None,
            &[wrong.clone(), right.clone()],
            preferred
        )
        .is_none()
    );
    let medium = candidate(Tier::Medium, Some(Effort::Low));
    let choice = seat(
        &QuartermasterModel::Auto,
        None,
        &[wrong, right, medium],
        preferred,
    )
    .unwrap();
    assert_eq!(choice.tier, Some(Tier::Medium));
    assert_eq!(choice.effort, Effort::Low);
}

#[test]
fn explicit_orchestrator_model_or_effort_keeps_separate_capped_semantics() {
    let tiers = TierEfforts::default();
    let mut c = candidate(Tier::Medium, Some(Effort::Max));
    c.efforts = vec![Effort::Low, Effort::High];
    let named = QuartermasterModel::Model(c.model.clone());
    // Named model without separate effort uses the tier default, not the selected combo.
    let choice = seat(&named, None, std::slice::from_ref(&c), None).unwrap();
    assert_eq!(choice.effort, Effort::Low); // medium default capped to supported set
    for setting in [QuartermasterModel::Auto, named] {
        let choice = seat(
            &setting,
            Some(Effort::ExtraHigh),
            std::slice::from_ref(&c),
            None,
        )
        .unwrap();
        assert_eq!(choice.effort, Effort::High);
    }
    c.effort_override = None;
    c.efforts = EFFORTS.to_vec();
    assert_eq!(
        seat(&QuartermasterModel::Auto, None, &[c], None)
            .unwrap()
            .effort,
        tier_default(&tiers, Tier::Medium)
    );
}

#[test]
fn resumed_effort_remains_fixed_even_with_combo_and_quick_overrides() {
    let choice = choose_effort(EffortInput {
        tier_default: Effort::ExtraHigh,
        placement_override: Some(Effort::Max),
        named: Some(Effort::High),
        quick_verification: true,
        supported: &[Effort::Low],
        resumed: Some(Effort::Medium),
    });
    assert_eq!(choice.effort, Effort::Medium);
    assert!(!choice.capped);
    assert!(!choice.stepped_down);
}
