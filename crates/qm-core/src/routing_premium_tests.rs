//! Issue #59 in the core: whenever premium is asked for, a premium model runs if any can serve,
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
            named_effort: None,
            premium: false,
            stronger_retry: None,
            later_step: None,
            prefer_provider_other_than: None,
        }
    }

    fn route(&self, facts: &CrewmateFacts) -> RouteOutcome {
        route_crewmate(
            &self.conn,
            &RuleTable::embedded().unwrap(),
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
fn a_named_premium_model_at_its_limit_runs_on_another_premium_model() {
    let f = Fixture::new("named-cut-off");
    f.cut_off(101);
    let mut facts = f.facts();
    facts.named_model = Some(id(FABLE));
    let record = routed(f.route(&facts));
    assert_eq!(used(&record), (Tier::Premium, GPT.to_owned()));
    assert_eq!(record.tier_landed, Tier::Premium);
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
        kind: TaskKind::Build,
        title: "feat(x): y".into(),
        goal: "g".into(),
        constraints: Vec::new(),
        tests_required: false,
        builds_on: None,
        instructions: Vec::new(),
        later_steps: Vec::new(),
        model: None,
        effort: None,
        premium: false,
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
