//! Premium routing: explicit models wait; tier-only requests retain premium fallback (#59).

use super::*;
use crate::rank::Usage;
use qm_types::domain::AccountKind;
use qm_types::ids::TaskId;

const FABLE: &str = "claude-fable-5-1";
const GPT: &str = "gpt-6-pro";
const OPUS: &str = "claude-opus-5-5";
const SONNET: &str = "claude-sonnet-5-5";

fn id(m: &str) -> ModelId {
    ModelId::new(m).unwrap()
}

/// One placement; each model gets its own account unless `account` says otherwise.
fn cand(model: &str, account: i64, tier: Tier) -> Candidate {
    Candidate {
        model: id(model),
        family: model.to_owned(),
        generation: 1,
        account: AccountId::new(account),
        account_kind: AccountKind::Subscription,
        tier,
        effort_override: None,
        // Opus-like: no Max, so premium's Extra high is kept and a cap is visible elsewhere.
        efforts: vec![Effort::Low, Effort::Medium, Effort::High, Effort::ExtraHigh],
        image_input: true,
        context_window: None,
        usage: Usage::Unknown,
        cut_off: false,
    }
}

fn cut(mut c: Candidate) -> Candidate {
    c.cut_off = true;
    c
}

fn no_image(mut c: Candidate) -> Candidate {
    c.image_input = false;
    c
}

struct Ask {
    user: UserChoices,
    later_step: Option<AppliedLaterStep>,
    retry: Option<Tier>,
    needs_image: bool,
    kind: TaskKind,
    premium_models: Vec<ModelId>,
}

impl Ask {
    fn new(kind: TaskKind) -> Self {
        Self {
            user: UserChoices::default(),
            later_step: None,
            retry: None,
            needs_image: false,
            kind,
            premium_models: Vec::new(),
        }
    }
    fn premium(mut self) -> Self {
        self.user.premium = true;
        self
    }
    fn model(mut self, m: &str) -> Self {
        self.user.model = Some(id(m));
        self
    }
    fn later(mut self, model: Option<&str>, premium: bool) -> Self {
        self.later_step = Some(AppliedLaterStep {
            from: TaskId::new(100).unwrap(),
            message: None,
            model: model.map(id),
            effort: None,
            premium,
        });
        self
    }
    fn retry(mut self, target: Tier) -> Self {
        self.user.stronger_retry = true;
        self.retry = Some(target);
        self
    }
}

fn run(ask: &Ask, placed: &[Candidate]) -> RoutingRecord {
    let table = RuleTable::from_kinds(&qm_types::config::KindTiers::default());
    let tiers = TierEfforts::default();
    let req = RouteRequest {
        kind: Decided::Answer {
            value: ask.kind,
            by: DecidedBy::Declared,
        },
        decider: DecidedBy::Declared,
        builds_on: None,
        needs: Needs {
            image_input: ask.needs_image,
            context_tokens: None,
        },
        quick_verification: false,
        user: ask.user.clone(),
        stronger_retry_target: ask.retry,
        later_step: ask.later_step.clone(),
        always_use: None,
        preferred_account: None,
        tiers: &tiers,
        premium_models: &ask.premium_models,
    };
    route(
        &table,
        &req,
        Pool {
            placed,
            pinned: &[],
        },
    )
}

fn used(r: &RoutingRecord) -> Option<(Tier, String)> {
    r.choice.as_ref().map(|c| (c.tier, c.model.to_string()))
}

/// The pool states the #59 guard runs over.
fn pool_states() -> Vec<(&'static str, Vec<Candidate>, bool)> {
    let opus = cand(OPUS, 3, Tier::High);
    let sonnet = cand(SONNET, 4, Tier::Medium);
    vec![
        (
            "premium serves",
            vec![
                cand(FABLE, 1, Tier::Premium),
                cand(GPT, 2, Tier::Premium),
                opus.clone(),
                sonnet.clone(),
            ],
            false,
        ),
        (
            "one premium cut off, another free",
            vec![
                cut(cand(FABLE, 1, Tier::Premium)),
                cand(GPT, 2, Tier::Premium),
                opus.clone(),
                sonnet.clone(),
            ],
            false,
        ),
        (
            "every premium cut off",
            vec![
                cut(cand(FABLE, 1, Tier::Premium)),
                cut(cand(GPT, 2, Tier::Premium)),
                opus.clone(),
                sonnet.clone(),
            ],
            false,
        ),
        ("premium empty", vec![opus.clone(), sonnet.clone()], false),
        (
            "premium lacks image input",
            vec![
                no_image(cand(FABLE, 1, Tier::Premium)),
                no_image(cand(GPT, 2, Tier::Premium)),
                opus.clone(),
                sonnet.clone(),
            ],
            true,
        ),
        (
            "every premium and high cut off",
            vec![
                cut(cand(FABLE, 1, Tier::Premium)),
                cut(cand(GPT, 2, Tier::Premium)),
                cut(opus),
                sonnet,
            ],
            false,
        ),
    ]
}

/// Every way of asking for premium; `None` for the named model means none is named.
fn ways(kind: TaskKind) -> Vec<(&'static str, Ask, Option<&'static str>)> {
    vec![
        ("the premium flag", Ask::new(kind).premium(), None),
        ("naming fable", Ask::new(kind).model(FABLE), Some(FABLE)),
        ("naming gpt", Ask::new(kind).model(GPT), Some(GPT)),
        (
            "the flag and naming fable",
            Ask::new(kind).premium().model(FABLE),
            Some(FABLE),
        ),
        (
            "a later step with premium",
            Ask::new(kind).later(None, true),
            None,
        ),
        (
            "a later step naming fable",
            Ask::new(kind).later(Some(FABLE), false),
            Some(FABLE),
        ),
        (
            "the flag over an older later step naming opus",
            Ask::new(kind).premium().later(Some(OPUS), false),
            None,
        ),
    ]
}

/// Named premium models must serve or wait. Tier-only requests still fall back with reasons.
#[test]
fn premium_runs_on_premium_whenever_any_premium_model_can_serve() {
    for &kind in TaskKind::ALL {
        for (state, placed, needs_image) in pool_states() {
            for (way, mut ask, named) in ways(kind) {
                ask.needs_image = needs_image;
                let ctx = format!("{kind} · {state} · {way}");
                let serves = |c: &Candidate| skip_reason(c, ask_needs(&ask)).is_none();
                // A named model that isn't placed at all is refused before routing.
                if named.is_some_and(|m| !placed.iter().any(|c| c.model == id(m))) {
                    continue;
                }
                let r = run(&ask, &placed);
                assert_eq!(r.tier_landed, Tier::Premium, "{ctx}");
                let premium_serving: Vec<&Candidate> = placed
                    .iter()
                    .filter(|c| c.tier == Tier::Premium && serves(c))
                    .collect();
                if let Some(m) = named
                    && !premium_serving.iter().any(|c| c.model == id(m))
                {
                    assert!(r.choice.is_none(), "{ctx}: {}", r.why);
                    assert!(r.fallbacks.is_empty(), "{ctx}");
                    assert!(r.why.contains(m), "{ctx}: {}", r.why);
                    assert!(r.why.contains("wait"), "{ctx}: {}", r.why);
                    continue;
                }
                let (tier, model) = used(&r).unwrap_or_else(|| panic!("{ctx}: waits"));
                if premium_serving.is_empty() {
                    let expected = [Tier::High, Tier::Medium, Tier::Low]
                        .into_iter()
                        .find(|t| placed.iter().any(|c| c.tier == *t && serves(c)))
                        .unwrap();
                    assert_eq!(tier, expected, "{ctx}");
                    assert_eq!(
                        r.fallbacks.first(),
                        Some(&FallbackStep {
                            from: Tier::Premium,
                            to: Tier::High
                        }),
                        "{ctx}"
                    );
                    assert!(
                        r.why.contains(&format!("so premium runs on {expected}")),
                        "{ctx}: {}",
                        r.why
                    );
                    for c in placed.iter().filter(|c| c.tier == Tier::Premium) {
                        let reason = skip_reason(c, ask_needs(&ask)).unwrap();
                        assert!(
                            r.why.contains(&format!("{}: {reason}", c.model)),
                            "{ctx}: {}",
                            r.why
                        );
                    }
                    // Premium's effort, capped by the model that runs.
                    assert_eq!(r.choice.unwrap().effort, Effort::ExtraHigh, "{ctx}");
                } else {
                    assert_eq!(tier, Tier::Premium, "{ctx}: {}", r.why);
                    assert!(r.fallbacks.is_empty(), "{ctx}");
                    if let Some(m) = named
                        && premium_serving.iter().any(|c| c.model == id(m))
                    {
                        assert_eq!(model, m, "{ctx}");
                    }
                }
            }
        }
    }
}

fn ask_needs(ask: &Ask) -> Needs {
    Needs {
        image_input: ask.needs_image,
        context_tokens: None,
    }
}

fn claude_pool(fable_cut: bool) -> Vec<Candidate> {
    let fable = cand(FABLE, 1, Tier::Premium);
    vec![
        if fable_cut { cut(fable) } else { fable },
        cand(OPUS, 1, Tier::High),
    ]
}

#[test]
fn case_a_premium_flag_runs_on_premium() {
    let r = run(&Ask::new(TaskKind::Build).premium(), &claude_pool(false));
    assert_eq!(used(&r), Some((Tier::Premium, FABLE.to_owned())));
    assert_eq!(r.why, "You asked for premium");
}

#[test]
fn case_b_premium_flag_with_premium_at_its_limit_runs_on_high_and_says_so() {
    let r = run(&Ask::new(TaskKind::Build).premium(), &claude_pool(true));
    assert_eq!(used(&r), Some((Tier::High, OPUS.to_owned())));
    assert_eq!(
        r.why,
        "You asked for premium. No premium model could serve (claude-fable-5-1: its account \
         is at its limit until it resets), so premium runs on high"
    );
    assert_eq!(
        r.fallbacks,
        vec![FallbackStep {
            from: Tier::Premium,
            to: Tier::High
        }]
    );
}

#[test]
fn case_c_premium_flag_with_premium_empty_runs_on_high_and_says_so() {
    let r = run(
        &Ask::new(TaskKind::Build).premium(),
        &[cand(OPUS, 1, Tier::High)],
    );
    assert_eq!(used(&r), Some((Tier::High, OPUS.to_owned())));
    assert_eq!(
        r.why,
        "You asked for premium. No model is placed in premium, so premium runs on high"
    );
}

#[test]
fn case_d_named_premium_model_at_its_limit_waits_even_with_other_premium_available() {
    let mut placed = claude_pool(true);
    for another_premium in [false, true] {
        if another_premium {
            placed.push(cand(GPT, 2, Tier::Premium));
        }
        let r = run(&Ask::new(TaskKind::Build).model(FABLE), &placed);
        assert_eq!(r.tier_landed, Tier::Premium);
        assert!(r.choice.is_none());
        assert_eq!(r.fallbacks, []);
        assert!(r.why.contains(FABLE), "{}", r.why);
        assert!(r.why.contains("at its limit"), "{}", r.why);
    }
}

#[test]
fn case_e_absent_named_model_waits_here_and_unplaced_models_are_refused_in_the_core() {
    let r = run(
        &Ask::new(TaskKind::Build).model(FABLE),
        &[cand(OPUS, 1, Tier::High)],
    );
    assert_eq!(r.tier_landed, Tier::High);
    assert!(r.choice.is_none());
    assert!(r.why.contains("no usable account"), "{}", r.why);
}

#[test]
fn a_named_premium_model_whose_login_broke_waits_for_that_model() {
    // The core leaves an unusable account's pairs out of the pool but lists premium's models.
    let mut ask = Ask::new(TaskKind::Build).model(FABLE);
    ask.premium_models = vec![id(FABLE)];
    let r = run(
        &ask,
        &[cand(GPT, 2, Tier::Premium), cand(OPUS, 3, Tier::High)],
    );
    assert_eq!(r.tier_landed, Tier::Premium);
    assert!(r.choice.is_none());
    assert_eq!(r.fallbacks, []);
}

#[test]
fn case_f_premium_on_this_turn_replaces_an_older_later_step() {
    let ask = Ask::new(TaskKind::Build).premium().later(Some(OPUS), false);
    let r = run(&ask, &claude_pool(false));
    assert_eq!(used(&r), Some((Tier::Premium, FABLE.to_owned())));
    assert_eq!(r.facts.later_step_from, None);
    assert_eq!(r.why, "You asked for premium");
}

#[test]
fn this_turns_words_replace_the_whole_later_step() {
    let mut placed = claude_pool(false);
    placed.push(cand(SONNET, 1, Tier::Medium));
    // An effort named now: the entry's model doesn't apply either.
    let mut ask = Ask::new(TaskKind::Build).later(Some(SONNET), false);
    ask.later_step.as_mut().unwrap().effort = Some(Effort::Low);
    ask.user.effort = Some(Effort::High);
    let r = run(&ask, &placed);
    assert_eq!(used(&r), Some((Tier::High, OPUS.to_owned())));
    assert_eq!(r.choice.unwrap().effort, Effort::High);
    assert_eq!(r.facts.later_step_from, None);
    // A model named now: the entry's effort doesn't apply.
    let mut ask = Ask::new(TaskKind::Build).model(SONNET).later(None, false);
    ask.later_step.as_mut().unwrap().effort = Some(Effort::Low);
    let r = run(&ask, &placed);
    assert_eq!(used(&r), Some((Tier::Medium, SONNET.to_owned())));
    assert_eq!(r.choice.unwrap().effort, Effort::Medium);
    // Nothing named now: the entry applies.
    let r = run(
        &Ask::new(TaskKind::Build).later(Some(SONNET), false),
        &placed,
    );
    assert_eq!(used(&r), Some((Tier::Medium, SONNET.to_owned())));
    assert_eq!(r.facts.later_step_from, TaskId::new(100));
}

#[test]
fn case_g_no_premium_asked_routes_by_the_rule_table() {
    let r = run(&Ask::new(TaskKind::Build), &claude_pool(false));
    assert_eq!(r.tier_landed, Tier::High);
    assert_eq!(used(&r), Some((Tier::High, OPUS.to_owned())));
}

#[test]
fn a_model_in_high_and_premium_named_for_premium_runs_as_premium() {
    let mut high = cand(OPUS, 1, Tier::High);
    high.effort_override = Some(Effort::Medium);
    let placed = vec![high, cand(OPUS, 1, Tier::Premium)];
    let r = run(&Ask::new(TaskKind::Lookup).model(OPUS), &placed);
    let c = r.choice.unwrap();
    assert_eq!((c.tier, c.effort), (Tier::Premium, Effort::ExtraHigh));
    assert_eq!(r.tier_landed, Tier::Premium);
}

#[test]
fn a_named_model_that_isnt_premium_also_waits() {
    let placed = vec![
        cand(FABLE, 1, Tier::Premium),
        cut(cand(OPUS, 2, Tier::High)),
        cand(SONNET, 3, Tier::Medium),
    ];
    let r = run(&Ask::new(TaskKind::Build).model(OPUS), &placed);
    assert_eq!(r.tier_landed, Tier::High);
    assert!(r.choice.is_none());
    assert_eq!(r.fallbacks, []);
}

#[test]
fn a_premium_later_step_applies_only_to_its_task() {
    let placed = claude_pool(false);
    let r = run(&Ask::new(TaskKind::Review).later(None, true), &placed);
    assert_eq!(used(&r), Some((Tier::Premium, FABLE.to_owned())));
    assert_eq!(r.facts.later_step_from, TaskId::new(100));
    // Without an entry, a task built on premium work routes by its kind.
    let r = run(&Ask::new(TaskKind::Review), &placed);
    assert_ne!(r.tier_landed, Tier::Premium);
}

#[test]
fn a_retry_to_premium_falls_back_and_says_so() {
    let r = run(
        &Ask::new(TaskKind::Build).retry(Tier::Premium),
        &claude_pool(true),
    );
    assert_eq!(used(&r), Some((Tier::High, OPUS.to_owned())));
    assert!(
        r.why
            .starts_with("You chose to try a stronger model. No premium model could serve"),
        "{}",
        r.why
    );
    assert!(r.facts.user.stronger_retry);
}

#[test]
fn a_retry_to_high_still_waits_for_high() {
    let placed = vec![
        cut(cand(OPUS, 1, Tier::High)),
        cand(SONNET, 2, Tier::Medium),
    ];
    let r = run(&Ask::new(TaskKind::Build).retry(Tier::High), &placed);
    assert_eq!(r.choice, None);
    assert_eq!(r.why, "waiting for a high model");
}

#[test]
fn premium_waits_only_when_no_tier_can_serve() {
    let placed = vec![
        cut(cand(FABLE, 1, Tier::Premium)),
        cut(cand(OPUS, 1, Tier::High)),
    ];
    let r = run(&Ask::new(TaskKind::Build).premium(), &placed);
    assert_eq!(r.choice, None);
    assert_eq!(r.why, "No model in any tier can serve this task; it waits");
}

#[test]
fn premium_skips_the_pin() {
    let table = RuleTable::from_kinds(&qm_types::config::KindTiers::default());
    let tiers = TierEfforts::default();
    let pin = AlwaysUse {
        model: id(SONNET),
        fallback: None,
    };
    let pinned = [cand(SONNET, 1, Tier::Medium)];
    let placed = claude_pool(false);
    for user in [
        UserChoices {
            premium: true,
            ..UserChoices::default()
        },
        UserChoices {
            model: Some(id(FABLE)),
            ..UserChoices::default()
        },
    ] {
        let req = RouteRequest {
            kind: Decided::Answer {
                value: TaskKind::Build,
                by: DecidedBy::Declared,
            },
            decider: DecidedBy::Declared,
            builds_on: None,
            needs: Needs::default(),
            quick_verification: false,
            user,
            stronger_retry_target: None,
            later_step: None,
            always_use: Some(&pin),
            preferred_account: None,
            tiers: &tiers,
            premium_models: &[],
        };
        let r = route(
            &table,
            &req,
            Pool {
                placed: &placed,
                pinned: &pinned,
            },
        );
        assert_eq!(used(&r), Some((Tier::Premium, FABLE.to_owned())));
    }
}
