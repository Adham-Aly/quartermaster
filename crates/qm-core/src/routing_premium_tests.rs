//! Premium routing in the core: an explicit model must serve or wait; otherwise premium may fall back.
//! a named model in no tier is refused, and a secondmate's start keeps the request's premium.

use super::*;
use crate::secondmates::scope::apply_request_overrides;
use qm_policy::overrides::Authority;
use qm_store::secondmates::{NewRequest, NewSecondmate, RequestOverrides, StoredLaterStep};
use qm_store::{connection, schema, seat, seq};
use qm_tools::defs::LaterStep;
use qm_tools::handler::{Caller, StartRequest};
use qm_types::data_dir::DataDir;
use qm_types::domain::{Effort, Role, StartedBy};
use qm_types::ids::{MessageId, SecondmateId, TurnId};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;

const FABLE: &str = "claude-fable-5-1";
const GPT: &str = "gpt-6-pro";
const OPUS: &str = "claude-opus-5-5";

fn id(m: &str) -> ModelId {
    ModelId::new(m).unwrap()
}

fn now() -> Timestamp {
    "2026-10-23T00:00:00Z".parse().unwrap()
}

/// A migrated database with three accounts: Fable (premium) on 101, GPT-6 Pro (premium) on 102,
/// Opus (high) on 103, and a task to route.
struct Fixture {
    dir: PathBuf,
    conn: Connection,
    qm: AgentId,
    task: TaskId,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "qm-core-premium-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut conn = connection::open_memory().unwrap();
        schema::migrate(&mut conn).unwrap();
        let qm = seat::ensure_quartermaster(&conn, &dir, now()).unwrap().id;
        conn.execute_batch(
            r#"
            INSERT INTO accounts (id, provider, label, kind, engine, created_at, seq) VALUES
              (101, 'anthropic', 'A', 'api_key', 'provider', 0, 10),
              (102, 'openai', 'B', 'api_key', 'provider', 0, 11),
              (103, 'anthropic', 'C', 'api_key', 'provider', 0, 12);
            INSERT INTO models (account_id, model, canonical, family, display_name, capabilities,
                efforts, first_seen_at, is_new, seq) VALUES
              (101, 'claude-fable-5-1', 'claude-fable-5-1', 'claude-fable', 'Fable',
               '{"image_input":true,"context_window":1000000,"generation":2,"known":true,"source":"account"}',
               '["low","medium","high","xhigh","max"]', 0, 0, 20),
              (102, 'gpt-6-pro', 'gpt-6-pro', 'gpt-6', 'GPT-6 Pro',
               '{"image_input":true,"context_window":400000,"generation":1,"known":true,"source":"account"}',
               '["low","medium","high","xhigh"]', 0, 0, 21),
              (103, 'claude-opus-5-5', 'claude-opus-5-5', 'claude-opus', 'Opus',
               '{"image_input":true,"context_window":1000000,"generation":1,"known":true,"source":"account"}',
               '["low","medium","high","xhigh","max"]', 0, 0, 22);
            INSERT INTO placements (canonical, tier, created_at, seq) VALUES
              ('claude-fable-5-1', 'premium', 0, 30),
              ('gpt-6-pro', 'premium', 0, 31),
              ('claude-opus-5-5', 'high', 0, 32);
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
        ).unwrap();
        let task: TaskId = conn
            .query_row(
                "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                     delivery_mode, branch, status, created_at, seq)
                 VALUES (100, ?1, ?2, 'research', 't', 'g', 'direct_pr', 'qm/task-0100',
                     'queued', 0, 50) RETURNING id",
                params![project, qm],
                |r| r.get(0),
            )
            .unwrap();
        Self {
            dir,
            conn,
            qm,
            task,
        }
    }

    /// Puts `account` at its limit until 2027.
    fn cut_off(&self, account: i64) {
        accounts::record_usage(
            &self.conn,
            AccountId::new(account),
            r#"{"windows":[{"name":"five_hour","used_percent":100.0,"resets_at":"2027-01-01T00:00:00Z"}],"read_at":"2026-10-23T00:00:00Z"}"#,
            now(),
        )
        .unwrap();
    }

    fn facts(&self) -> CrewmateFacts {
        CrewmateFacts {
            task: self.task,
            agent: None,
            authority: Authority::Turn(StartedBy::User {
                message: MessageId::new(1),
            }),
            kind: Decided::Answer {
                value: TaskKind::Research,
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

    fn route(&self, facts: &CrewmateFacts) -> RouteOutcome {
        route_crewmate(
            &self.conn,
            &Config::default(),
            facts,
            &LoginHealth::new(),
            now(),
        )
        .unwrap()
    }

    fn decisions(&self) -> i64 {
        self.conn
            .query_row("SELECT count(*) FROM routing_decisions", [], |r| r.get(0))
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn routed(outcome: RouteOutcome) -> RoutingRecord {
    match outcome {
        RouteOutcome::Routed(r) => r.record,
        other => panic!("expected a routed crewmate, got {other:?}"),
    }
}

fn used(record: &RoutingRecord) -> (Tier, String) {
    let c = record.choice.as_ref().unwrap();
    (c.tier, c.model.to_string())
}

/// A lookup with usable low and medium placements makes tier versus effort observable.
fn lookup_fixture(name: &str) -> (Fixture, CrewmateFacts) {
    let f = Fixture::new(name);
    f.conn
        .execute_batch(
            "UPDATE placements SET tier = 'low' WHERE canonical = 'claude-fable-5-1';
             UPDATE placements SET tier = 'medium' WHERE canonical = 'gpt-6-pro';",
        )
        .unwrap();
    let mut facts = f.facts();
    facts.kind = Decided::Answer {
        value: TaskKind::Lookup,
        by: DecidedBy::Declared,
    };
    (f, facts)
}

#[test]
fn explicit_medium_tier_lookup_routes_medium_and_records_tier() {
    let (f, mut facts) = lookup_fixture("tier-medium");
    facts.named_tier = Some(Tier::Medium);
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::Medium, GPT.to_owned()));
    assert_eq!(record.facts.user.tier, Some(Tier::Medium));
    let stored: String = f
        .conn
        .query_row("SELECT facts FROM routing_decisions", [], |row| row.get(0))
        .unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["user"]["tier"], "medium");
}

#[test]
fn medium_effort_lookup_stays_low() {
    let (f, mut facts) = lookup_fixture("effort-medium");
    facts.named_effort = Some(Effort::Medium);
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::Low, FABLE.to_owned()));
    assert_eq!(record.choice.unwrap().effort, Effort::Medium);
}

#[test]
fn explicit_tier_on_app_event_is_refused_without_recording() {
    let (f, mut facts) = lookup_fixture("tier-app-event");
    facts.authority = Authority::Turn(StartedBy::AppEvent);
    facts.named_tier = Some(Tier::Medium);
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    assert_eq!(f.decisions(), 0);
}

#[test]
fn explicit_tier_matching_retry_target_is_allowed() {
    let f = Fixture::new("tier-retry-match");
    let mut facts = f.facts();
    facts.named_tier = Some(Tier::High);
    facts.stronger_retry = Some(Tier::High);
    assert_eq!(
        used(&routed(f.route(&facts))),
        (Tier::High, OPUS.to_owned())
    );
    assert_eq!(f.decisions(), 1);
}

#[test]
fn explicit_tier_conflicting_with_retry_target_is_refused_without_recording() {
    for unavailable in [false, true] {
        let f = Fixture::new("tier-retry-conflict");
        if unavailable {
            // Premium can serve, but that must not override the computed high retry target.
            f.cut_off(103);
        }
        let mut facts = f.facts();
        facts.named_tier = Some(Tier::Premium);
        facts.stronger_retry = Some(Tier::High);
        match f.route(&facts) {
            RouteOutcome::Refused(r) => {
                assert!(r.reason.contains("premium"));
                assert!(r.reason.contains("high"));
                assert!(r.reason.contains("retry"));
            }
            other => panic!("expected a retry tier conflict refusal, got {other:?}"),
        }
        assert_eq!(f.decisions(), 0);
    }
}

#[test]
fn matching_high_retry_waits_when_only_premium_can_serve() {
    let f = Fixture::new("tier-retry-match-unavailable");
    f.cut_off(103);
    let mut facts = f.facts();
    facts.named_tier = Some(Tier::High);
    facts.stronger_retry = Some(Tier::High);
    assert!(matches!(f.route(&facts), RouteOutcome::Waiting { .. }));
    assert_eq!(f.decisions(), 1);
}

#[test]
fn explicit_tier_bypasses_always_use_pin() {
    let (f, mut facts) = lookup_fixture("tier-pin");
    facts.named_tier = Some(Tier::Medium);
    let mut config = Config::default();
    config.routing.always_use = Some(AlwaysUse {
        model: id(OPUS),
        fallback: None,
    });
    let record =
        routed(route_crewmate(&f.conn, &config, &facts, &LoginHealth::new(), now()).unwrap());
    assert_eq!(used(&record), (Tier::Medium, GPT.to_owned()));
}

#[test]
fn named_model_outside_explicit_tier_is_refused_after_canonicalization() {
    let (f, mut facts) = lookup_fixture("tier-model");
    f.conn
        .execute(
            "UPDATE models SET model = 'account-fable' WHERE canonical = ?1",
            [FABLE],
        )
        .unwrap();
    facts.named_model = Some(id("account-fable"));
    facts.named_tier = Some(Tier::Medium);
    match f.route(&facts) {
        RouteOutcome::Refused(r) => {
            assert!(r.reason.contains(FABLE));
            assert!(r.reason.contains("medium"));
            assert!(r.reason.contains("Settings ▸ Model levels"));
        }
        other => panic!("expected a tier conflict refusal, got {other:?}"),
    }
    assert_eq!(f.decisions(), 0);
    // Placement membership, not availability or capability, is the core's validation boundary.
    f.conn.execute("INSERT INTO placements (canonical, tier, created_at, seq) VALUES (?1, 'medium', 0, 60)", [FABLE]).unwrap();
    assert_eq!(
        used(&routed(f.route(&facts))),
        (Tier::Medium, FABLE.to_owned())
    );
}

#[test]
fn premium_conflicts_with_an_explicit_nonpremium_tier() {
    let (f, mut facts) = lookup_fixture("tier-premium-conflict");
    facts.named_tier = Some(Tier::Medium);
    facts.premium = true;
    match f.route(&facts) {
        RouteOutcome::Refused(r) => assert!(r.reason.contains("premium conflicts")),
        other => panic!("expected a premium conflict refusal, got {other:?}"),
    }
    assert_eq!(f.decisions(), 0);
}

#[test]
fn preference_aliases_are_canonicalized_and_persisted() {
    let f = Fixture::new("preference-alias");
    f.conn
        .execute(
            "UPDATE models SET model = 'account-opus' WHERE canonical = ?1",
            [OPUS],
        )
        .unwrap();
    let mut facts = f.facts();
    facts.preference = Some(qm_types::model_preference::ModelPreference {
        selection: id("account-opus"),
        provider: Some(Provider::Anthropic),
        model: Some(id("account-opus")),
        effort: Some(Effort::ExtraHigh),
    });
    let RouteOutcome::Routed(result) = f.route(&facts) else {
        panic!("preference should route");
    };
    assert_eq!(used(&result.record), (Tier::High, OPUS.to_owned()));
    assert_eq!(result.account_model, id("account-opus"));
    let preference = result.record.facts.preference.as_ref().unwrap();
    assert_eq!(preference.selection, id(OPUS));
    assert_eq!(preference.model, Some(id(OPUS)));
    assert!(result.record.why.contains("Your model preference selected"));
    let stored: String = f
        .conn
        .query_row("SELECT facts FROM routing_decisions", [], |row| row.get(0))
        .unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["preference"]["selection"], OPUS);
    assert_eq!(stored["preference"]["model"], OPUS);
}

#[test]
fn stale_preference_is_not_refused_and_never_enables_premium() {
    let f = Fixture::new("preference-stale");
    for selection in ["missing-model", FABLE] {
        let mut facts = f.facts();
        facts.preference = Some(qm_types::model_preference::ModelPreference {
            selection: id(selection),
            provider: None,
            model: None,
            effort: None,
        });
        let record = routed(f.route(&facts));
        assert_eq!(used(&record), (Tier::High, OPUS.to_owned()));
        assert!(record.why.contains("normal routing was used"));
        assert!(!record.facts.user.premium);
    }
    assert_eq!(f.decisions(), 2);
}

#[test]
fn a_named_model_in_no_tier_is_refused_and_nothing_is_recorded() {
    let f = Fixture::new("no-tier");
    let mut facts = f.facts();
    facts.named_model = Some(id("claude-nowhere-1"));
    match f.route(&facts) {
        RouteOutcome::Refused(r) => assert_eq!(
            r.reason,
            "claude-nowhere-1 isn't in any model level; place it in Settings ▸ Model levels or \
             name another model."
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(f.decisions(), 0);
}

#[test]
fn a_later_steps_model_in_no_tier_is_refused_too() {
    let f = Fixture::new("later-no-tier");
    let mut facts = f.facts();
    facts.later_step = Some(AppliedLaterStep {
        from: TaskId::new(7).unwrap(),
        message: None,
        model: Some(id("claude-nowhere-1")),
        effort: None,
        premium: false,
    });
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    assert_eq!(f.decisions(), 0);
}

#[test]
fn placed_named_model_without_an_account_waits_then_routes_when_it_returns() {
    for from_later in [false, true] {
        let f = Fixture::new("placed-unavailable");
        let requested = id("gpt-6.1-sol");
        // A retained placement can outlive every account/model row that offered it.
        f.conn
            .execute(
                "INSERT INTO placements (canonical, tier, effort_override, created_at, seq)
             VALUES (?1, 'high', 'high', 0, 60)",
                [&requested],
            )
            .unwrap();
        let mut facts = f.facts();
        facts.authority = Authority::Turn(StartedBy::AppEvent);
        if from_later {
            facts.later_step = Some(AppliedLaterStep {
                from: f.task,
                message: Some(MessageId::new(1)),
                model: Some(requested.clone()),
                effort: Some(Effort::High),
                premium: false,
            });
        } else {
            facts.choice_message = Some(MessageId::new(1));
            facts.named_model = Some(requested.clone());
            facts.named_effort = Some(Effort::High);
        }
        assert_eq!(models::offered(&f.conn, &requested).unwrap(), []);
        // Opus is usable, so this is not merely the empty-pool waiting path.
        assert_eq!(
            used(&routed(f.route(&f.facts()))),
            (Tier::High, OPUS.to_owned())
        );
        let RouteOutcome::Waiting { decision, record } = f.route(&facts) else {
            panic!("a placed but unavailable named model must wait")
        };
        assert!(record.choice.is_none());
        assert_eq!(record.fallbacks, []);
        assert!(record.why.contains("gpt-6.1-sol"), "{}", record.why);
        assert!(record.why.contains("no usable account"), "{}", record.why);
        let saved = routing_decisions::decisions_for_task(&f.conn, f.task).unwrap();
        let (saved_id, saved_record) = saved.last().unwrap();
        assert_eq!(*saved_id, decision);
        assert!(saved_record.choice.is_none());
        assert_eq!(saved_record.why, record.why);

        let account = accounts::insert_account(
            &f.conn,
            &accounts::NewAccount {
                provider: Provider::OpenAi,
                label: "returned".into(),
                kind: qm_types::domain::AccountKind::ApiKey,
                credential_ref: None,
                base_url: None,
                at: now(),
            },
        )
        .unwrap();
        let model = ModelFacts {
            model: requested.clone(),
            canonical: requested.clone(),
            display_name: None,
            family: "gpt-6".into(),
            generation: 1,
            image_input: true,
            context_window: 400_000,
            efforts: vec![Effort::High],
            known: true,
        };
        models::record_model_list(&f.conn, account, ListSource::Account, &[model], now()).unwrap();
        let rerouted = routed(f.route(&facts));
        let choice = rerouted.choice.unwrap();
        assert_eq!(choice.model, requested);
        assert_eq!(choice.account, account);
        assert_eq!(choice.effort, Effort::High);
        assert_eq!(rerouted.fallbacks, []);
        assert_eq!(f.decisions(), 3);
    }
}

#[test]
fn a_named_premium_model_at_its_limit_waits_for_that_model() {
    let f = Fixture::new("named-cut-off");
    f.cut_off(101);
    let mut facts = f.facts();
    facts.named_model = Some(id(FABLE));
    let RouteOutcome::Waiting { record, .. } = f.route(&facts) else {
        panic!("an explicit named model must not be substituted")
    };
    assert!(record.choice.is_none());
    assert_eq!(record.fallbacks, []);
    assert!(record.why.contains("at its limit"), "{}", record.why);
    assert_eq!(record.tier_landed, Tier::Premium);
}

#[test]
fn explicit_review_models_outrank_provider_diversification() {
    for later in [false, true] {
        let f = Fixture::new("review-explicit-model");
        let mut facts = f.facts();
        facts.kind = Decided::Answer {
            value: TaskKind::Review,
            by: DecidedBy::Declared,
        };
        facts.prefer_provider_other_than = Some(Provider::Anthropic);
        if later {
            facts.later_step = Some(AppliedLaterStep {
                from: TaskId::new(7).unwrap(),
                message: None,
                model: Some(id(FABLE)),
                effort: None,
                premium: false,
            });
        } else {
            facts.named_model = Some(id(FABLE));
        }
        let record = routed(f.route(&facts));
        assert_eq!(used(&record), (Tier::Premium, FABLE.to_owned()));
        assert_eq!(
            record.facts.prefer_provider_other_than,
            Some(Provider::Anthropic)
        );
    }
}

/// Two high-tier providers, with the builder's Anthropic model preferred by ordinary ranking.
fn review_fixture(name: &str) -> (Fixture, CrewmateFacts) {
    let f = Fixture::new(name);
    f.conn
        .execute(
            "UPDATE placements SET tier = 'high' WHERE canonical = ?1",
            [GPT],
        )
        .unwrap();
    let mut facts = f.facts();
    facts.kind = Decided::Answer {
        value: TaskKind::Review,
        by: DecidedBy::Declared,
    };
    facts.preference = Some(qm_types::model_preference::ModelPreference {
        selection: id(OPUS),
        provider: Some(Provider::Anthropic),
        model: Some(id(OPUS)),
        effort: None,
    });
    (f, facts)
}

#[test]
fn stale_and_unavailable_review_preferences_restore_diversification() {
    for unavailable in [false, true] {
        let (f, mut facts) = review_fixture("review-invalid-preference");
        if unavailable {
            f.conn
                .execute(
                    "UPDATE placements SET tier = 'high' WHERE canonical = ?1",
                    [FABLE],
                )
                .unwrap();
            f.cut_off(101);
            let preference = facts.preference.as_mut().unwrap();
            preference.selection = id(FABLE);
            preference.model = Some(id(FABLE));
        } else {
            facts.preference.as_mut().unwrap().selection = id("missing-model");
        }
        facts.prefer_provider_other_than = Some(Provider::Anthropic);
        let record = routed(f.route(&facts));
        assert_eq!(used(&record), (Tier::High, GPT.to_owned()));
        assert!(!record.facts.preference_applied);
        assert_eq!(record.facts.preference, facts.preference);
        assert!(record.why.contains("preference did not select an entry"));
        assert!(record.why.contains("normal review routing was used"));
    }
}

#[test]
fn valid_review_preference_outranks_provider_diversification() {
    let (f, mut facts) = review_fixture("review-valid-preference");
    facts.prefer_provider_other_than = Some(Provider::Anthropic);
    let mut ordinary = facts.clone();
    ordinary.preference = None;
    assert_eq!(
        used(&routed(f.route(&ordinary))),
        (Tier::High, GPT.to_owned())
    );
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::High, OPUS.to_owned()));
    assert!(record.facts.preference_applied);
    assert_eq!(record.facts.preference, facts.preference);
    assert!(!record.why.contains("normal review routing was used"));
}

#[test]
fn inherited_effort_suppresses_preference_but_keeps_review_diversification() {
    let (f, mut facts) = review_fixture("review-inherited-effort");
    facts.prefer_provider_other_than = Some(Provider::Anthropic);
    facts.later_step = Some(AppliedLaterStep {
        from: TaskId::new(7).unwrap(),
        message: None,
        model: None,
        effort: Some(Effort::Low),
        premium: false,
    });
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::High, GPT.to_owned()));
    assert_eq!(record.choice.as_ref().unwrap().effort, Effort::Low);
    assert!(!record.facts.preference_applied);
    assert_eq!(record.facts.preference, facts.preference);
    assert!(record.why.contains("normal review routing was used"));
}

#[test]
fn premium_with_every_premium_model_cut_off_runs_on_high_and_says_why() {
    let f = Fixture::new("all-cut-off");
    f.cut_off(101);
    f.cut_off(102);
    let mut facts = f.facts();
    facts.premium = true;
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::High, OPUS.to_owned()));
    assert_eq!(
        record.why,
        "You asked for premium. No premium model could serve (claude-fable-5-1: its account is \
         at its limit until it resets; gpt-6-pro: its account is at its limit until it resets), \
         so premium runs on high"
    );
    assert_eq!(record.choice.unwrap().effort, Effort::ExtraHigh);
    assert_eq!(f.decisions(), 1);
}

#[test]
fn the_premium_models_are_found_through_placements() {
    let f = Fixture::new("tiers-of");
    assert_eq!(tiers_of(&f.conn, &id(FABLE)).unwrap(), vec![Tier::Premium]);
    assert_eq!(tiers_of(&f.conn, &id(OPUS)).unwrap(), vec![Tier::High]);
    assert_eq!(
        tiers_of(&f.conn, &id("claude-nowhere-1")).unwrap(),
        Vec::new()
    );
}

// Tool boundary --------------------------------------------------------------------------------

mod tier_tool_tests {
    use super::*;
    use qm_tools::defs::{RoutingSummary, TaskStarted};
    use qm_tools::handler::{Dispatcher, ToolCx, ToolFailure, ToolHost, TurnCx};
    use qm_types::BoxFuture;
    use qm_types::clock::SystemClock;
    use qm_types::ids::TaskLabelId;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Host(Mutex<Vec<(Role, StartRequest)>>);

    impl ToolHost for Host {
        fn start_crewmate<'a>(
            &'a self,
            caller: &'a Caller,
            request: StartRequest,
        ) -> BoxFuture<'a, Result<TaskStarted, ToolFailure>> {
            self.0.lock().unwrap().push((caller.role, request));
            Box::pin(async {
                Ok(TaskStarted {
                    crewmate: TaskId::new(1).unwrap(),
                    task: TaskLabelId::new(1).unwrap(),
                    branch: None,
                    routing: RoutingSummary {
                        tier: Tier::Medium,
                        model: None,
                        effort: Some(Effort::High),
                        why: "test host".into(),
                    },
                })
            })
        }
    }

    #[tokio::test]
    async fn both_start_tools_forward_tier_and_effort_only_on_user_turns() {
        for role in [Role::Quartermaster, Role::Secondmate] {
            for started_by in [
                None,
                Some(StartedBy::AppEvent),
                Some(StartedBy::User {
                    message: MessageId::new(1),
                }),
            ] {
                let host = Host::default();
                let caller = Caller {
                    agent: AgentId::new(1),
                    role,
                    turn: started_by.map(|started_by| TurnCx {
                        turn: TurnId::new(1),
                        started_by,
                    }),
                    task: None,
                    secondmate: None,
                    plan: None,
                };
                let allowed = matches!(started_by, Some(StartedBy::User { .. }));
                // On refused turns, tier is the only override: effort cannot mask a missing check.
                let effort = allowed.then_some(Effort::High);
                let cx = ToolCx {
                    caller: &caller,
                    clock: &SystemClock,
                    host: &host,
                };
                let result = Dispatcher::builtin()
                    .dispatch(
                        &cx,
                        "start_crewmate",
                        serde_json::json!({
                            "project": "p", "kind": "lookup", "title": "docs(x): look up x",
                            "goal": "look up x", "tier": "medium", "effort": effort
                        }),
                    )
                    .await;
                assert_eq!(result.result.is_error, !allowed, "{role:?} {started_by:?}");
                let calls = host.0.lock().unwrap();
                assert_eq!(calls.len(), usize::from(allowed));
                if let Some((forwarded_role, request)) = calls.first() {
                    assert_eq!(*forwarded_role, role);
                    assert_eq!(request.tier, Some(Tier::Medium));
                    assert_eq!(request.effort, Some(Effort::High));
                }
            }
        }
    }
}

// Secondmates -----------------------------------------------------------------------------------

/// A secondmate with one request carrying `overrides`.
fn request(f: &Fixture, overrides: &RequestOverrides) -> (Caller, StartRequest) {
    let data_dir = DataDir::new(&f.dir);
    let name = format!("Web {}", rand::random::<u32>());
    let sm = qm_store::secondmates::insert(
        &f.conn,
        &NewSecondmate {
            name: &name,
            scope: "web",
            projects: &[],
            standing_branch: None,
            parent: f.qm,
            data_dir: &data_dir,
            at: now(),
        },
    )
    .unwrap();
    // The Quartermaster's routing turn; it has one open turn at a time.
    let turn: TurnId = f
        .conn
        .query_row(
            "SELECT id FROM turns WHERE agent_id = ?1",
            params![f.qm],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
        .unwrap_or_else(|| {
            f.conn
                .query_row(
                    "INSERT INTO turns (agent_id, started_by, started_at, seq)
                     VALUES (?1, 'app_event', 0, ?2) RETURNING id",
                    params![f.qm, seq::next(now())],
                    |r| r.get(0),
                )
                .unwrap()
        });
    let correlation = qm_store::secondmates::insert_request(
        &f.conn,
        &NewRequest {
            secondmate: sm.id,
            turn,
            request: "r",
            user_words: "",
            instructions: &[],
            base_branch: None,
            overrides,
            at: now(),
        },
    )
    .unwrap();
    let caller = secondmate_caller(sm.agent, sm.id);
    let start = StartRequest {
        folder: None,
        project: Some("p".into()),
        in_place: false,
        create_folder: false,
        label: None,
        kind: TaskKind::Build,
        title: "feat(x): y".into(),
        name: None,
        goal: "g".into(),
        constraints: Vec::new(),
        tests_required: false,
        builds_on: None,
        instructions: Vec::new(),
        later_steps: Vec::new(),
        model: None,
        preference: None,
        tier: None,
        effort: None,
        premium: false,
        choice_message: None,
        base_branch: None,
        verification: None,
        stronger_retry_of: None,
        correlation: Some(correlation),
    };
    (caller, start)
}

fn secondmate_caller(agent: AgentId, secondmate: SecondmateId) -> Caller {
    Caller {
        agent,
        role: Role::Secondmate,
        turn: None,
        task: None,
        secondmate: Some(secondmate),
        plan: None,
    }
}

fn premium_request() -> RequestOverrides {
    RequestOverrides {
        premium: true,
        ..RequestOverrides::default()
    }
}

#[test]
fn the_requests_premium_survives_an_effort_the_secondmate_typed() {
    let f = Fixture::new("sm-effort");
    let (caller, mut start) = request(&f, &premium_request());
    start.effort = Some(Effort::High);
    let authority = apply_request_overrides(&f.conn, &caller, &mut start)
        .unwrap()
        .unwrap();
    assert!(authority.is_some());
    assert!(start.premium);
    assert_eq!(start.effort, Some(Effort::High));
}

#[test]
fn every_crewmate_under_a_premium_request_is_premium() {
    let f = Fixture::new("sm-every");
    let (caller, start) = request(&f, &premium_request());
    for kind in [TaskKind::Plan, TaskKind::Build, TaskKind::Review] {
        let mut start = start.clone();
        start.kind = kind;
        apply_request_overrides(&f.conn, &caller, &mut start)
            .unwrap()
            .unwrap();
        assert!(start.premium, "{kind}");
    }
}

#[test]
fn a_requests_premium_later_step_is_added_per_kind_and_typed_kinds_win() {
    let f = Fixture::new("sm-later");
    let overrides = RequestOverrides {
        later_steps: vec![
            StoredLaterStep {
                kind: TaskKind::Build,
                model: None,
                effort: None,
                premium: true,
            },
            StoredLaterStep {
                kind: TaskKind::Review,
                model: None,
                effort: Some(Effort::Low),
                premium: false,
            },
        ],
        ..RequestOverrides::default()
    };
    let (caller, mut start) = request(&f, &overrides);
    start.later_steps = vec![LaterStep {
        kind: TaskKind::Review,
        model: None,
        effort: Some(Effort::High),
        premium: false,
    }];
    apply_request_overrides(&f.conn, &caller, &mut start)
        .unwrap()
        .unwrap();
    assert!(!start.premium);
    assert_eq!(
        start.later_steps,
        vec![
            LaterStep {
                kind: TaskKind::Review,
                model: None,
                effort: Some(Effort::High),
                premium: false,
            },
            LaterStep {
                kind: TaskKind::Build,
                model: None,
                effort: None,
                premium: true,
            },
        ]
    );
}

#[test]
fn a_secondmate_typing_a_model_outside_premium_on_a_premium_request_is_refused() {
    let f = Fixture::new("sm-refused");
    for overrides in [
        premium_request(),
        RequestOverrides {
            model: Some(id(FABLE)),
            ..RequestOverrides::default()
        },
    ] {
        let (caller, mut start) = request(&f, &overrides);
        start.model = Some(id(OPUS));
        let refusal = apply_request_overrides(&f.conn, &caller, &mut start)
            .unwrap()
            .unwrap_err();
        assert!(
            refusal.reason.contains("the request asked for premium"),
            "{}",
            refusal.reason
        );
        // A premium model is fine.
        let (caller, mut start) = request(&f, &overrides);
        start.model = Some(id(GPT));
        assert!(
            apply_request_overrides(&f.conn, &caller, &mut start)
                .unwrap()
                .is_ok()
        );
    }
}

#[test]
fn earlier_actual_user_choices_work_on_a_later_app_event_turn() {
    let f = Fixture::new("lasting-user-source");
    let mut facts = f.facts();
    facts.authority = Authority::Turn(StartedBy::AppEvent);
    facts.named_model = Some(id(FABLE));
    facts.named_effort = Some(Effort::High);
    facts.premium = true;
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    assert_eq!(f.decisions(), 0);
    facts.choice_message = Some(MessageId::new(1));
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::Premium, FABLE.to_owned()));
    assert_eq!(record.choice.unwrap().effort, Effort::High);
}

#[test]
fn an_orchestrators_user_turn_assertion_is_not_user_message_provenance() {
    let f = Fixture::new("forged-user-source");
    let mut facts = f.facts();
    facts.named_effort = Some(Effort::High);
    facts.authority = Authority::Turn(StartedBy::User {
        message: MessageId::new(-1),
    });
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    facts.authority = Authority::Turn(StartedBy::AppEvent);
    facts.choice_message = Some(MessageId::new(-1));
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    assert_eq!(f.decisions(), 0);
}

#[test]
fn delegated_choices_keep_their_source_and_cannot_authorize_different_fields() {
    let f = Fixture::new("delegated-user-source");
    let overrides = RequestOverrides {
        model: Some(id(FABLE)),
        effort: Some(Effort::High),
        choice_message: Some(MessageId::new(1)),
        ..RequestOverrides::default()
    };
    let (caller, mut start) = request(&f, &overrides);
    // Chat retention nulls the original turn after restart; the request must still load and carry
    // its actual-user citation rather than treating its later app-event turn as the authority.
    f.conn
        .execute(
            "UPDATE secondmate_requests SET turn_id = NULL WHERE id = ?1",
            [start.correlation],
        )
        .unwrap();
    apply_request_overrides(&f.conn, &caller, &mut start)
        .unwrap()
        .unwrap();
    assert_eq!(start.choice_message, overrides.choice_message);
    f.conn
        .execute(
            "UPDATE tasks SET supervisor_id = ?1, secondmate_request_id = ?2 WHERE id = ?3",
            params![caller.agent, start.correlation, f.task],
        )
        .unwrap();
    let mut facts = f.facts();
    facts.authority = Authority::Turn(StartedBy::AppEvent);
    facts.choice_message = start.choice_message;
    facts.named_model = start.model;
    facts.named_effort = start.effort;
    assert_eq!(
        used(&routed(f.route(&facts))),
        (Tier::Premium, FABLE.to_owned())
    );
    facts.named_model = Some(id(GPT));
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    facts.named_model = Some(id(FABLE));
    facts.premium = true;
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
}

fn delegated_preference(model: &str) -> qm_types::model_preference::ModelPreference {
    qm_types::model_preference::ModelPreference {
        selection: id(model),
        provider: None,
        model: None,
        effort: Some(Effort::High),
    }
}

#[test]
fn delegated_tier_and_preference_reach_every_start_after_chat_retention() {
    let f = Fixture::new("sm-tier-preference");
    let overrides = RequestOverrides {
        tier: Some(Tier::Premium),
        preference: Some(delegated_preference(FABLE)),
        choice_message: Some(MessageId::new(1)),
        ..RequestOverrides::default()
    };
    let (caller, start) = request(&f, &overrides);
    f.conn
        .execute(
            "UPDATE secondmate_requests SET turn_id = NULL WHERE id = ?1",
            [start.correlation],
        )
        .unwrap();
    for kind in [TaskKind::Plan, TaskKind::Build, TaskKind::Review] {
        let mut child = start.clone();
        child.kind = kind;
        assert!(
            apply_request_overrides(&f.conn, &caller, &mut child)
                .unwrap()
                .unwrap()
                .is_some()
        );
        assert_eq!(child.tier, overrides.tier);
        assert_eq!(child.preference, overrides.preference);
        assert_eq!(child.choice_message, overrides.choice_message);
        // Reapplying cannot mutate a fully inherited start or claim more authority.
        let inherited = child.clone();
        assert!(
            apply_request_overrides(&f.conn, &caller, &mut child)
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert_eq!(child, inherited);
    }
}

#[test]
fn delegated_tier_and_preference_preserve_child_fields_and_source() {
    let f = Fixture::new("sm-child-choices");
    let overrides = RequestOverrides {
        tier: Some(Tier::High),
        preference: Some(delegated_preference(OPUS)),
        effort: Some(Effort::Low),
        ..RequestOverrides::default()
    };
    let (caller, mut start) = request(&f, &overrides);
    start.tier = Some(Tier::Premium);
    start.preference = Some(delegated_preference(GPT));
    start.model = Some(id(FABLE));
    start.effort = Some(Effort::High);
    start.premium = true;
    start.choice_message = Some(MessageId::new(42));
    let typed = start.clone();
    assert!(
        apply_request_overrides(&f.conn, &caller, &mut start)
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(start, typed);
    // Inheriting just one field must not replace the child's provenance or named model.
    start.preference = None;
    assert!(
        apply_request_overrides(&f.conn, &caller, &mut start)
            .unwrap()
            .unwrap()
            .is_some()
    );
    assert_eq!(start.preference, overrides.preference);
    assert_eq!(start.choice_message, typed.choice_message);
    assert_eq!(start.model, typed.model);
    assert_eq!(start.tier, typed.tier);
    assert_eq!(start.effort, typed.effort);
    assert!(start.premium);
}

#[test]
fn tier_choices_require_an_actual_user_source_but_preferences_do_not() {
    let f = Fixture::new("tier-source");
    let mut facts = f.facts();
    facts.authority = Authority::Turn(StartedBy::AppEvent);
    facts.named_tier = Some(Tier::Premium);
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    facts.choice_message = Some(MessageId::new(-1));
    assert!(matches!(f.route(&facts), RouteOutcome::Refused(_)));
    assert_eq!(f.decisions(), 0);
    facts.choice_message = Some(MessageId::new(1));
    assert_eq!(used(&routed(f.route(&facts))).0, Tier::Premium);
    facts.named_tier = None;
    facts.choice_message = None;
    facts.preference = Some(delegated_preference(OPUS));
    assert_eq!(
        used(&routed(f.route(&facts))),
        (Tier::High, OPUS.to_owned())
    );
}
