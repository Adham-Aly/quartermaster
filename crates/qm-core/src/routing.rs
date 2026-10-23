//! Routing in the core: model detection and the routed choice for every crewmate.
//!
//! The rules themselves are plain functions in `qm-routing` and `qm-policy`; this module feeds
//! them from the store and records what they decide. Four entry points:
//!
//! # 1. Detection, in the background — [`spawn_detection`]
//!
//! At startup the binary calls [`spawn_detection`] once. It returns a `JoinHandle` at once, so
//! **detection never blocks startup**; each active account's list is read concurrently in its own
//! task, so a slow account never holds up another. For each account ([`detect_account`]):
//!
//! 1. its list is read through `Engine::models`, bounded by [`DETECTION_TIMEOUT`] on the app's
//!    clock;
//! 2. a list that was read is matched to the catalog ([`ModelCatalog::resolve`]; unknown models
//!    get cautious defaults and are marked unknown) and reconciled with the store
//!    (`qm_store::models::record_model_list`): new models are marked new, "not in use" and
//!    announced once; gone models are announced once and keep their placement;
//! 3. a failed or timed-out read falls back to **the last list read** (left as stored), or, when
//!    there is none, to **the built-in catalog's list** (recorded as a stand-in that announces
//!    nothing);
//!
//! Detection never assigns tiers: new installs start empty, and saved placements stay unchanged.
//!
//! # 2. Routing a crewmate — [`route_crewmate`] (for `tasks.rs`)
//!
//! ```text
//! route_crewmate(conn, &Config, &CrewmateFacts, at) -> Result<RouteOutcome, StoreError>
//! ```
//!
//! Call it inside the transaction that starts the crewmate, after the `tasks` row exists (the
//! decision row references it). [`CrewmateFacts`] carries the declared facts, the authority of
//! the turn (`qm_policy::overrides::Authority`: a turn started by your message, an app event, or a
//! card choice), anything you named (model, effort, premium, a stronger-model retry), an applicable
//! `later_steps` entry, and what the task needs. It returns:
//!
//! - [`RouteOutcome::Refused`] when an override arrives on a turn that can't carry it (a named
//!   model on an app-event turn: "a named model needs a turn started by the user's own message"),
//!   or a model named for the task or by its later step is placed in no tier ("… isn't in any
//!   model level; place it in Settings ▸ Model levels or name another model"). Nothing is
//!   recorded; the refusal goes back to the agent as the tool's answer.
//! - [`RouteOutcome::Routed`] with the persisted `routing_decisions` row, the `RoutingRecord` (its
//!   `why` line is the one the task's record shows), the account's own id for the model (what the
//!   `SessionSpec` names), the provider (the engine follows it) and whether the catalog knew the
//!   model. An unknown model's "why" line says it runs with cautious defaults.
//! - [`RouteOutcome::Waiting`] with the persisted row when nothing can serve yet; route again when
//!   the pool changes.
//!
//! Precedence and every rule (named model first, later steps, premium and retries, the "always
//! use" pin with its fallback, the rule table, fallbacks, ranking, effort) are
//! `qm_routing::choose::route`'s, unchanged. The request lists every model placed in premium, so a
//! named premium model whose account's login broke still counts as asking for premium. Ask the
//! `Decider` first, only when `qm_routing::choose::decider_needed` says so, and pass its answer in
//! `kind`.
//!
//! # 3. The "always use" notice — [`always_use_notice`]
//!
//! "'Always use' stays as a setting on the pool, with its fallback model and a notice while it's
//! on": the line the bridge shows while the pin is set. When the pin's fallback serves, the routing
//! record's "why" line says so.
//!
//! # 4. The Quartermaster's own model — [`quartermaster_choice`]
//!
//! The setting (Auto by default) applied to the current pool through
//! `qm_routing::quartermaster_model::choose`, with your effort for it when you set one. Otherwise
//! Auto uses the selected entry's effort (the tier default for a legacy entry); a named model
//! uses its tier default. Routing never changes either setting.
//!
//! # Logins that stopped working
//!
//! [`route_crewmate`] and [`quartermaster_choice`] take the app's `qm_auth::LoginHealth` (the one
//! the provider engine's `TokenKeeper` records into) and leave out of the pool every account
//! whose login isn't usable, before `qm_routing::choose::route` sees it: one broken login
//! sidelines only its own account. A crewmate's routing record lists each pair left out in
//! `skipped`, with the login's problem as the reason. An account without a stored credential (a
//! Claude subscription, whose login `claude` keeps) is never left out here.
//!
//! # The catalog seam — [`ModelCatalog`]
//!
//! The catalog lives in `qm-providers` (`qm_providers::catalog`), which the core must not depend
//! on (it never names a concrete engine). The binary injects it through [`ModelCatalog`]:
//!
//! ```text
//! #[derive(Debug)] struct Catalog(&'static qm_providers::catalog::Catalog);
//! impl qm_core::routing::ModelCatalog for Catalog {
//!     fn resolve(&self, p: Provider, d: &DetectedModel) -> ModelFacts {
//!         let r = qm_providers::catalog::detect::resolve(self.0, p, d);
//!         ModelFacts { model: r.id, canonical: r.canonical, display_name: r.display_name,
//!                      family: r.family, generation: r.generation, image_input: r.image_input,
//!                      context_window: r.context_window, efforts: r.efforts, known: r.known }
//!     }
//!     fn catalog_list(&self, p: Provider) -> Vec<DetectedModel> {
//!         qm_providers::catalog::detect::catalog_list(self.0, p)
//!     }
//!     fn starter_set(&self, p: Provider) -> StarterTiers {
//!         let s = qm_providers::catalog::starter::starter_set(p);
//!         StarterTiers { approved: s.approved, placements: s.placements.iter()
//!             .filter_map(|x| Some((x.tier, ModelId::new(x.canonical).ok()?))).collect() }
//!     }
//! }
//! ```

use jiff::Timestamp;
use qm_auth::{CredentialRef, LoginHealth};
use qm_engine::{AccountRef, DetectedModel, Engine, UsageReading};
use qm_policy::overrides::{Authority, OverrideKind, check_override};
use qm_routing::choose::{Pool, RouteRequest, route};
use qm_routing::decider::{Decided, DecidedBy};
use qm_routing::later_steps::{AppliedLaterStep, not_in_any_tier};
use qm_routing::quartermaster_model::{self, SeatChoice};
use qm_routing::rank::{Candidate, Needs, Usage};
use qm_routing::record::{BuildsOn, RoutingRecord, SkippedModel, UserChoices};
use qm_routing::rules::RuleTable;
use qm_store::StoreError;
use qm_store::accounts::{self, AccountRow};
use qm_store::db::Db;
use qm_store::models::{self, ListSource, ModelFacts, PoolRow, Recorded};
use qm_store::routing_decisions::{self, DecisionChoice, RoutingDecision};
use qm_types::clock::Clock;
use qm_types::config::{AlwaysUse, Config, QuartermasterModel};
use qm_types::domain::{EngineKind, Provider, TaskKind, Tier};
use qm_types::error::Refusal;
use qm_types::ids::{AccountId, AgentId, ModelId, RoutingDecisionId, TaskId};
use rusqlite::Connection;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// The catalog seam
// ---------------------------------------------------------------------------------------------

/// A provider's starter tier set, as the catalog defines it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StarterTiers {
    /// `false` for a draft pending your approval.
    pub approved: bool,
    /// (tier, canonical model id).
    pub placements: Vec<(Tier, ModelId)>,
}

/// The model catalog, injected by the binary (see the module docs).
pub trait ModelCatalog: Send + Sync + fmt::Debug {
    /// Matches a detected model to the catalog; unknown models get cautious defaults and `known:
    /// false`.
    fn resolve(&self, provider: Provider, detected: &DetectedModel) -> ModelFacts;
    /// The catalog's own list for a provider, the last-resort fallback.
    fn catalog_list(&self, provider: Provider) -> Vec<DetectedModel>;
    /// The provider's starter tier set.
    fn starter_set(&self, provider: Provider) -> StarterTiers;
}

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

/// How long one account's list may take before the fallback applies.
///
/// Reads never block startup, but a read that hangs would otherwise leave the account with no
/// list at all, so a read gives up after 30 seconds.
pub const DETECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The engines the binary injects, by kind.
pub type Engines = HashMap<EngineKind, Arc<dyn Engine>>;

/// Which list an account's detection ended up using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionSource {
    /// The account's own list, read now.
    Account,
    /// The read failed; the last list read stays in use.
    LastList,
    /// The read failed and no list was ever recorded: the built-in catalog's list.
    Catalog,
}

/// What detection did for one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectionOutcome {
    pub account: AccountId,
    pub source: DetectionSource,
    /// What was recorded and announced (`None` when the last list stayed in use).
    pub recorded: Option<Recorded>,
    /// Starter placements added (always 0; detection no longer seeds presets).
    pub seeded: usize,
    /// Why the read failed, in plain words, when it did.
    pub read_error: Option<String>,
}

/// The `AccountRef` an engine needs for a stored account (the credential is a key, never the
/// secret).
pub fn account_ref(row: &AccountRow) -> AccountRef {
    AccountRef {
        id: row.id,
        provider: row.provider,
        credential: row.credential_ref.clone(),
        base_url: row.base_url.clone(),
    }
}

/// Reads every active account's model list in the background and returns at once. Each account
/// runs in its own task. Needs a Tokio runtime.
pub fn spawn_detection(
    db: Db,
    engines: Engines,
    catalog: Arc<dyn ModelCatalog>,
    clock: Arc<dyn Clock>,
) -> tokio::task::JoinHandle<Result<Vec<DetectionOutcome>, StoreError>> {
    tokio::spawn(async move {
        let rows = db.call(|conn| accounts::list_active(conn)).await?;
        let mut tasks = Vec::new();
        for row in rows {
            let Some(engine) = engines.get(&row.engine).cloned() else {
                continue;
            };
            let (db, catalog, clock) = (db.clone(), catalog.clone(), clock.clone());
            tasks.push(tokio::spawn(async move {
                detect_account(&db, engine.as_ref(), &row, catalog.as_ref(), clock.as_ref()).await
            }));
        }
        let mut out = Vec::with_capacity(tasks.len());
        for t in tasks {
            match t.await {
                Ok(outcome) => out.push(outcome?),
                Err(e) => return Err(StoreError::Panicked(e.to_string())),
            }
        }
        Ok(out)
    })
}

/// Reads and records one account's list, with fallbacks, without changing tier placements.
pub async fn detect_account(
    db: &Db,
    engine: &dyn Engine,
    account: &AccountRow,
    catalog: &dyn ModelCatalog,
    clock: &dyn Clock,
) -> Result<DetectionOutcome, StoreError> {
    let reference = account_ref(account);
    let read = tokio::select! {
        r = engine.models(&reference) => r.map_err(|e| e.to_string()),
        () = clock.sleep(DETECTION_TIMEOUT) => Err(format!(
            "the model list didn't arrive within {} seconds", DETECTION_TIMEOUT.as_secs()
        )),
    };
    let provider = account.provider;
    let id = account.id;
    let (list, read_error) = match read {
        Ok(detected) => (
            Some((
                ListSource::Account,
                detected
                    .iter()
                    .map(|d| catalog.resolve(provider, d))
                    .collect::<Vec<_>>(),
            )),
            None,
        ),
        Err(e) => (None, Some(e)),
    };
    let fallback: Vec<ModelFacts> = if list.is_none() {
        catalog
            .catalog_list(provider)
            .iter()
            .map(|d| catalog.resolve(provider, d))
            .collect()
    } else {
        Vec::new()
    };
    let at = clock.now();
    db.call(move |conn| {
        let tx = conn.transaction()?;
        let (source, recorded) = if let Some((src, facts)) = list {
            (
                DetectionSource::Account,
                Some(models::record_model_list(&tx, id, src, &facts, at)?),
            )
        } else {
            let has_last = models::list_models(&tx, id)?
                .iter()
                .any(|r| r.gone_at.is_none());
            if has_last {
                (DetectionSource::LastList, None)
            } else {
                (
                    DetectionSource::Catalog,
                    Some(models::record_model_list(
                        &tx,
                        id,
                        ListSource::Catalog,
                        &fallback,
                        at,
                    )?),
                )
            }
        };
        tx.commit()?;
        Ok(DetectionOutcome {
            account: id,
            source,
            recorded,
            seeded: 0,
            read_error,
        })
    })
    .await
}

/// Seeds a provider's starter tier set the first time it's connected; never again, and never over a
/// placement that exists.
pub fn seed_starter_tiers(
    conn: &Connection,
    provider: Provider,
    starter: &StarterTiers,
    at: Timestamp,
) -> Result<usize, StoreError> {
    models::seed_starter_placements(conn, provider, starter.approved, &starter.placements, at)
}

// ---------------------------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------------------------

/// A used percentage at or above this means the account is at its limit (cut off until reset).
const CUT_OFF_PERCENT: f64 = 100.0;

/// The tightest window of a stored usage reading; unreadable means unknown, never "all left".
/// Returns the usage and whether the account is cut off.
fn usage_of(json: Option<&str>, now: Timestamp) -> (Usage, bool) {
    let Some(reading) = json.and_then(|j| serde_json::from_str::<UsageReading>(j).ok()) else {
        return (Usage::Unknown, false);
    };
    let tightest = reading
        .windows
        .iter()
        .filter(|w| w.used_percent.is_finite())
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent));
    let Some(w) = tightest else {
        return (Usage::Unknown, false);
    };
    let resets_in = w
        .resets_at
        .and_then(|r| Duration::try_from(r.duration_since(now)).ok())
        .unwrap_or(Duration::ZERO);
    let cut_off = w.used_percent >= CUT_OFF_PERCENT && !resets_in.is_zero();
    (
        Usage::Known {
            used: (w.used_percent / 100.0).clamp(0.0, 1.0),
            resets_in,
        },
        cut_off,
    )
}

/// The pool as `qm-routing` sees it, plus what the core needs to start the session.
struct LoadedPool {
    placed: Vec<Candidate>,
    pinned: Vec<Candidate>,
    rows: Vec<PoolRow>,
    /// Pairs left out because their account's login isn't usable.
    unusable: Vec<SkippedModel>,
}

impl LoadedPool {
    /// The stored row behind a chosen (canonical model, account) pair.
    fn row(&self, canonical: &ModelId, account: AccountId) -> Option<&PoolRow> {
        self.rows
            .iter()
            .find(|r| &r.facts.canonical == canonical && r.account == account)
    }
}

fn candidate(row: &PoolRow, now: Timestamp) -> Candidate {
    let (usage, cut_off) = usage_of(row.usage.as_deref(), now);
    Candidate {
        model: row.facts.canonical.clone(),
        family: row.facts.family.clone(),
        generation: row.facts.generation,
        account: row.account,
        account_kind: row.account_kind,
        // A pair outside any tier (the pin) has its tier ignored by routing.
        tier: row.tier.unwrap_or(Tier::High),
        effort_override: row.effort_override,
        efforts: row.facts.efforts.clone(),
        image_input: row.facts.image_input,
        context_window: Some(row.facts.context_window),
        usage,
        cut_off,
    }
}

/// A model id as you or the settings named it, as the canonical id placements use: the account's
/// own id (`anthropic/claude-opus-5-5`) maps to its canonical one.
fn canonical(conn: &Connection, model: &ModelId) -> Result<ModelId, StoreError> {
    Ok(models::canonical_of(conn, model)?.unwrap_or_else(|| model.clone()))
}

/// The tiers `model` (or the model it's another name for) is placed in, for checking a named model.
pub(crate) fn tiers_of(conn: &Connection, model: &ModelId) -> Result<Vec<Tier>, StoreError> {
    let model = canonical(conn, model)?;
    Ok(models::list_placements(conn)?
        .into_iter()
        .filter(|p| p.canonical == model)
        .map(|p| p.tier)
        .collect())
}

/// The login problem of each active account whose login isn't usable.
fn unusable_accounts(
    conn: &Connection,
    health: &LoginHealth,
) -> Result<HashMap<AccountId, String>, StoreError> {
    Ok(accounts::list_active(conn)?
        .into_iter()
        .filter_map(|row| {
            let key = CredentialRef::new(row.credential_ref.as_deref()?).ok()?;
            if health.is_usable(&key) {
                return None;
            }
            let problem = health
                .problem(&key)
                .unwrap_or_else(|| "its login isn't usable".to_owned());
            Some((row.id, problem))
        })
        .collect())
}

fn load_pool(
    conn: &Connection,
    pin: Option<&AlwaysUse>,
    extra: &[ModelId],
    health: &LoginHealth,
    now: Timestamp,
) -> Result<LoadedPool, StoreError> {
    let unusable_accounts = unusable_accounts(conn, health)?;
    let mut unusable = Vec::new();
    let mut usable = |rows: Vec<PoolRow>| -> Vec<PoolRow> {
        rows.into_iter()
            .filter(|r| match unusable_accounts.get(&r.account) {
                Some(problem) => {
                    let skipped = SkippedModel {
                        model: r.facts.canonical.clone(),
                        account: r.account,
                        reason: format!("the account's login isn't usable: {problem}"),
                    };
                    if !unusable.contains(&skipped) {
                        unusable.push(skipped);
                    }
                    false
                }
                None => true,
            })
            .collect()
    };
    let placed_rows = usable(models::pool(conn)?);
    let mut pinned_rows = Vec::new();
    let mut wanted: Vec<&ModelId> = extra.iter().collect();
    if let Some(p) = pin {
        wanted.push(&p.model);
        wanted.extend(p.fallback.iter());
    }
    for m in wanted {
        pinned_rows.extend(usable(models::offered(conn, m)?));
    }
    let placed = placed_rows.iter().map(|r| candidate(r, now)).collect();
    let pinned = pinned_rows.iter().map(|r| candidate(r, now)).collect();
    let mut rows = placed_rows;
    rows.extend(pinned_rows);
    Ok(LoadedPool {
        placed,
        pinned,
        rows,
        unusable,
    })
}

fn canonical_pin(
    conn: &Connection,
    pin: Option<&AlwaysUse>,
) -> Result<Option<AlwaysUse>, StoreError> {
    pin.map(|p| {
        Ok(AlwaysUse {
            model: canonical(conn, &p.model)?,
            fallback: p
                .fallback
                .as_ref()
                .map(|f| canonical(conn, f))
                .transpose()?,
        })
    })
    .transpose()
}

// ---------------------------------------------------------------------------------------------
// Routing a crewmate
// ---------------------------------------------------------------------------------------------

/// Everything routing reads for one crewmate start.
#[derive(Clone, Debug)]
pub struct CrewmateFacts {
    /// The task being started; its row must exist.
    pub task: TaskId,
    /// The crewmate's agent, if already created; otherwise set later with
    /// `qm_store::routing_decisions::set_decision_agent`.
    pub agent: Option<AgentId>,
    /// Who started the turn (or a card choice) the start came from.
    pub authority: Authority,
    /// The Decider's answer, or the declared kind as an answer when it was skipped.
    pub kind: Decided<TaskKind>,
    /// Who was asked, recorded when the answer is undecided.
    pub decider: DecidedBy,
    /// What the work builds on, with the fact the app checked.
    pub builds_on: Option<BuildsOn>,
    pub needs: Needs,
    /// The task runs at the `quick` verification level.
    pub quick_verification: bool,
    /// A model you named for this task.
    pub named_model: Option<ModelId>,
    /// An effort level you named for this task.
    pub named_effort: Option<qm_types::domain::Effort>,
    /// You asked for premium.
    pub premium: bool,
    /// A stronger-model retry you chose, with its target tier from `qm_policy::holds`.
    pub stronger_retry: Option<Tier>,
    /// The applicable `later_steps` entry (`qm_routing::later_steps::applicable`), applied whatever
    /// turn starts the task.
    pub later_step: Option<AppliedLaterStep>,
    /// A review: rank models of another provider than the builder's first, within the landed tier.
    /// `None` routes as usual.
    pub prefer_provider_other_than: Option<Provider>,
}

/// A routed crewmate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routed {
    pub decision: RoutingDecisionId,
    pub record: RoutingRecord,
    /// The id the account uses for the model: what the `SessionSpec` names.
    pub account_model: ModelId,
    /// The account's provider; the engine follows it.
    pub provider: Provider,
    /// The catalog knew the model; `false` means cautious defaults.
    pub known: bool,
}

/// What [`route_crewmate`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteOutcome {
    Routed(Box<Routed>),
    /// Nothing can serve yet; the row is recorded.
    Waiting {
        decision: RoutingDecisionId,
        record: Box<RoutingRecord>,
    },
    /// An override the turn couldn't carry. Nothing is recorded.
    Refused(Refusal),
}

/// Refuses any override present on a turn that can't carry it, first.
fn check_overrides(facts: &CrewmateFacts) -> Result<(), Refusal> {
    let present = [
        (facts.named_model.is_some(), OverrideKind::NamedModel),
        (facts.premium, OverrideKind::Premium),
        (facts.named_effort.is_some(), OverrideKind::Effort),
        (
            facts.stronger_retry.is_some(),
            OverrideKind::StrongerModelRetry,
        ),
    ];
    for (_, kind) in present.iter().filter(|(on, _)| *on) {
        check_override(facts.authority, *kind)?;
    }
    Ok(())
}

/// The note an unknown model's routing record carries.
fn unknown_note(model: &ModelId) -> String {
    format!(
        "{model} isn't in the catalog yet, so it runs with cautious defaults (no image input, a \
         modest context window)"
    )
}

fn to_value<T: serde::Serialize>(
    column: &'static str,
    v: &T,
) -> Result<serde_json::Value, StoreError> {
    serde_json::to_value(v).map_err(|source| StoreError::Json { column, source })
}

/// Routes over the pool; with `avoid`, models of another provider than `avoid` come first within
/// the landed tier (a review), and the whole pool is used when none of them
/// can serve there.
fn route_preferring(
    table: &RuleTable,
    request: &RouteRequest<'_>,
    pool: &LoadedPool,
    avoid: Option<Provider>,
) -> RoutingRecord {
    let full = Pool {
        placed: &pool.placed,
        pinned: &pool.pinned,
    };
    let Some(avoid) = avoid else {
        return route(table, request, full);
    };
    // `placed[i]` was built from `rows[i]`, so the rows say each candidate's provider.
    let others: Vec<Candidate> = pool
        .placed
        .iter()
        .zip(&pool.rows)
        .filter(|(_, row)| row.provider != avoid)
        .map(|(c, _)| c.clone())
        .collect();
    let preferred = route(
        table,
        request,
        Pool {
            placed: &others,
            pinned: &pool.pinned,
        },
    );
    let in_landed_tier = preferred.fallbacks.is_empty()
        && preferred
            .choice
            .as_ref()
            .is_some_and(|c| c.tier == preferred.tier_landed);
    if in_landed_tier {
        preferred
    } else {
        route(table, request, full)
    }
}

/// Routes one crewmate and records the decision (see the module docs). Call inside the transaction
/// that starts the crewmate. Accounts whose login `health` marks unusable are left out of the pool
/// and listed in the record's `skipped`.
pub fn route_crewmate(
    conn: &Connection,
    config: &Config,
    facts: &CrewmateFacts,
    health: &LoginHealth,
    at: Timestamp,
) -> Result<RouteOutcome, StoreError> {
    if let Err(refusal) = check_overrides(facts) {
        return Ok(RouteOutcome::Refused(refusal));
    }

    let named_model = facts
        .named_model
        .as_ref()
        .map(|m| canonical(conn, m))
        .transpose()?;
    let later_step = facts
        .later_step
        .clone()
        .map(|mut l| {
            l.model = l.model.as_ref().map(|m| canonical(conn, m)).transpose()?;
            Ok::<_, StoreError>(l)
        })
        .transpose()?;
    // A model you name is one placed in some tier; routing never quietly picks another.
    let placements = models::list_placements(conn)?;
    for model in named_model
        .iter()
        .chain(later_step.as_ref().and_then(|l| l.model.as_ref()))
    {
        if !placements.iter().any(|p| &p.canonical == model) {
            return Ok(RouteOutcome::Refused(not_in_any_tier(model)));
        }
    }
    let premium_models: Vec<ModelId> = placements
        .into_iter()
        .filter(|p| p.tier == Tier::Premium)
        .map(|p| p.canonical)
        .collect();
    let pin = canonical_pin(conn, config.routing.always_use.as_ref())?;
    let pool = load_pool(conn, pin.as_ref(), &[], health, at)?;
    let preferred = accounts::preferred_account(conn)?;

    let request = RouteRequest {
        kind: facts.kind.clone(),
        decider: facts.decider.clone(),
        builds_on: facts.builds_on.clone(),
        needs: facts.needs,
        quick_verification: facts.quick_verification,
        user: UserChoices {
            model: named_model,
            effort: facts.named_effort,
            premium: facts.premium,
            stronger_retry: facts.stronger_retry.is_some(),
        },
        stronger_retry_target: facts.stronger_retry,
        later_step,
        always_use: pin.as_ref(),
        preferred_account: preferred,
        tiers: &config.tiers,
        premium_models: &premium_models,
    };
    // The table follows your Task kinds settings as they are now.
    let table = RuleTable::from_kinds(&config.routing.kinds);
    let mut record = route_preferring(&table, &request, &pool, facts.prefer_provider_other_than);
    record.facts.prefer_provider_other_than = facts.prefer_provider_other_than;
    record.skipped.extend(pool.unusable.iter().cloned());

    let chosen = record
        .choice
        .as_ref()
        .and_then(|c| pool.row(&c.model, c.account))
        .map(|row| (row.facts.model.clone(), row.provider, row.facts.known));
    if let (Some(choice), Some((_, _, false))) = (&record.choice, &chosen) {
        record.why = format!("{}. {}", record.why, unknown_note(&choice.model));
    }

    let decision = routing_decisions::insert_decision(
        conn,
        &RoutingDecision {
            task: facts.task,
            agent: facts.agent,
            facts: to_value("routing_decisions.facts", &record.facts)?,
            rule: record.rule.clone(),
            decided_by: record.decided_by.to_string(),
            tier_landed: record.tier_landed,
            choice: record.choice.as_ref().map(|c| DecisionChoice {
                tier: c.tier,
                model: c.model.clone(),
                account: c.account,
                effort: c.effort,
            }),
            skipped: to_value("routing_decisions.skipped", &record.skipped)?,
            fallbacks: to_value("routing_decisions.fallbacks", &record.fallbacks)?,
            why: record.why.clone(),
            at,
        },
    )?;

    Ok(match chosen {
        Some((account_model, provider, known)) => RouteOutcome::Routed(Box::new(Routed {
            decision,
            record,
            account_model,
            provider,
            known,
        })),
        None => RouteOutcome::Waiting {
            decision,
            record: Box::new(record),
        },
    })
}

/// The tiers with a usable model now: a placement whose account's login works and isn't cut off
/// at its limit (choice 2 of a decision card).
pub fn usable_tiers(
    conn: &Connection,
    health: &LoginHealth,
    now: Timestamp,
) -> Result<Vec<Tier>, StoreError> {
    let pool = load_pool(conn, None, &[], health, now)?;
    let mut tiers: Vec<Tier> = pool
        .placed
        .iter()
        .filter(|c| !c.cut_off)
        .map(|c| c.tier)
        .collect();
    tiers.sort_unstable();
    tiers.dedup();
    Ok(tiers)
}

// ---------------------------------------------------------------------------------------------
// The "always use" notice
// ---------------------------------------------------------------------------------------------

/// The notice shown while the "always use" pin is on; `None` when it's off.
pub fn always_use_notice(config: &Config) -> Option<String> {
    config.routing.always_use.as_ref().map(|pin| match &pin.fallback {
        Some(f) => format!(
            "Always use is on: crew work runs on {}, or {f} when it can't serve, ignoring tiers",
            pin.model
        ),
        None => format!(
            "Always use is on: crew work runs on {}, ignoring tiers",
            pin.model
        ),
    })
}

// ---------------------------------------------------------------------------------------------
// The Quartermaster's own model
// ---------------------------------------------------------------------------------------------

/// The orchestrators' model choice, with what the core needs to open the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatModel {
    pub choice: SeatChoice,
    /// The id the account uses for the model.
    pub account_model: ModelId,
    pub provider: Provider,
}

/// Applies the Quartermaster's model setting (Auto by default) to the current pool. Secondmates
/// follow the same setting. `None` when nothing can serve. Apply a change at the next idle moment,
/// never mid-turn (`qm_routing::quartermaster_model::switch_kind`). Accounts whose login `health`
/// marks unusable are left out.
pub fn quartermaster_choice(
    conn: &Connection,
    config: &Config,
    health: &LoginHealth,
    at: Timestamp,
) -> Result<Option<SeatModel>, StoreError> {
    let setting = match &config.quartermaster.model {
        QuartermasterModel::Auto => QuartermasterModel::Auto,
        QuartermasterModel::Model(m) => QuartermasterModel::Model(canonical(conn, m)?),
    };
    let extra: Vec<ModelId> = match &setting {
        QuartermasterModel::Model(m) => vec![m.clone()],
        QuartermasterModel::Auto => Vec::new(),
    };
    let pool = load_pool(conn, None, &extra, health, at)?;
    // A named model may run on any account that serves it, placed or not.
    let mut candidates = pool.placed.clone();
    candidates.extend(pool.pinned.iter().cloned());
    let preferred = accounts::preferred_account(conn)?;
    let Some(choice) = quartermaster_model::choose(
        &setting,
        config.quartermaster.effort,
        &candidates,
        preferred,
        &config.tiers,
    ) else {
        return Ok(None);
    };
    Ok(pool
        .row(&choice.model, choice.account)
        .map(|row| SeatModel {
            account_model: row.facts.model.clone(),
            provider: row.provider,
            choice: choice.clone(),
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_engine::scripted::{Script, ScriptedEngine};
    use qm_routing::record::FallbackStep;
    use qm_store::accounts::NewAccount;
    use qm_types::clock::SystemClock;
    use qm_types::domain::{AccountKind, Effort};

    fn routing_fixture() -> (Connection, AccountRow, ModelFacts) {
        let mut conn = qm_store::connection::open_memory().unwrap();
        qm_store::schema::migrate(&mut conn).unwrap();
        // Only routing's foreign-key parents: no filesystem, engine, or real app state.
        conn.execute_batch(
            "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode,
                 merge_method, created_at, seq)
             VALUES (1, 'routing', '/fixture', 'created', 'main', 'direct_pr', 'merge', 0, 1);
             INSERT INTO agents (id, role, home, status, created_at, seq)
             VALUES (1, 'quartermaster', '/fixture/seat', 'idle', 0, 1);
             INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal,
                 delivery_mode, branch, status, created_at, seq)
             VALUES (1, 1, 1, 'build', 'routing', 'routing', 'direct_pr', 'test', 'running', 0, 1);",
        )
        .unwrap();
        let account = accounts::insert_account(
            &conn,
            &NewAccount {
                provider: Provider::Claude,
                label: "routing".into(),
                kind: AccountKind::Subscription,
                credential_ref: None,
                base_url: None,
                at: Timestamp::UNIX_EPOCH,
            },
        )
        .unwrap();
        let account = accounts::load_account(&conn, account).unwrap().unwrap();
        let mut model = Catalog.resolve(
            Provider::Claude,
            &DetectedModel {
                id: ModelId::new("account-model").unwrap(),
                display_name: None,
                efforts: Vec::new(),
                image_input: None,
                context_window: None,
            },
        );
        model.canonical = ModelId::new("canonical-model").unwrap();
        model.efforts = vec![Effort::Low, Effort::Medium, Effort::High, Effort::Max];
        models::record_model_list(
            &conn,
            account.id,
            ListSource::Account,
            std::slice::from_ref(&model),
            Timestamp::UNIX_EPOCH,
        )
        .unwrap();
        (conn, account, model)
    }

    fn assert_session_choice(routed: &Routed, account: &AccountRow, model: &ModelFacts) {
        use crate::tasks::session::{RoutedSession, Told, Workplace, crewmate_spec};
        use qm_types::data_dir::DataDir;
        use qm_types::ids::PortBlock;

        let choice = routed.record.choice.as_ref().unwrap();
        assert_eq!(choice.model, model.canonical);
        assert_eq!(choice.account, account.id);
        assert_eq!(routed.account_model, model.model);
        assert_eq!(routed.provider, account.provider);
        let paths =
            DataDir::with_scratch("/fixture", "/fixture/scratch").task(TaskId::new(1).unwrap());
        let spec = crewmate_spec(
            AgentId::new(2),
            &RoutedSession {
                engine: account.engine,
                account: account_ref(account),
                model: routed.account_model.clone(),
                effort: choice.effort,
            },
            &paths,
            &Workplace {
                dir: paths.worktree.clone(),
            },
            PortBlock { base: 10_000 },
            Told {
                gathered: &crate::skills::Gathered::default(),
                plan: None,
            },
            None,
        );
        assert_eq!(spec.model, model.model);
        assert_eq!(spec.account.id, choice.account);
        assert_eq!(spec.effort, choice.effort);
    }

    #[test]
    fn stored_and_migrated_combos_reach_core_decisions_and_session_specs() {
        let table = RuleTable::parse(
            r#"rules = [
                { kind = "lookup", tier = "low" },
                { kind = "build", tier = "medium" },
                { kind = "*", tier = "high" },
            ]"#,
        )
        .unwrap();
        for legacy in [false, true] {
            let (mut conn, account, model) = routing_fixture();
            // Non-default settings prove migration uses loaded config, not built-in defaults.
            let mut config: Config =
                toml::from_str("[tiers]\nhigh = 'max'\nmedium = 'high'").unwrap();
            let tx = conn.transaction().unwrap();
            for (tier, effort) in [(Tier::High, Effort::Max), (Tier::Medium, Effort::High)] {
                models::place_model(
                    &tx,
                    &model.canonical,
                    None,
                    (!legacy).then_some(effort),
                    tier,
                    Timestamp::UNIX_EPOCH,
                )
                .unwrap();
            }
            assert_eq!(
                models::migrate_legacy_efforts(&tx, &config.tiers).unwrap(),
                if legacy { 2 } else { 0 }
            );
            tx.commit().unwrap();
            let saved = models::list_placements(&conn).unwrap();
            assert_eq!(saved.len(), 2);
            assert_eq!(saved[0].effort_override, Some(Effort::Max));
            assert_eq!(saved[1].effort_override, Some(Effort::High));
            // Once explicit, combos must survive changed defaults, including low's fallback.
            config.tiers.high = Effort::Low;
            config.tiers.medium = Effort::Low;
            config.tiers.low = Effort::Low;
            let health = LoginHealth::default();
            let seat = quartermaster_choice(&conn, &config, &health, Timestamp::UNIX_EPOCH)
                .unwrap()
                .unwrap();
            assert_eq!(seat.account_model, model.model);
            assert_eq!(seat.choice.effort, Effort::Max);
            for (kind, landed, used, effort) in [
                (TaskKind::Unclear, Tier::High, Tier::High, Effort::Max),
                (TaskKind::Build, Tier::Medium, Tier::Medium, Effort::High),
                (TaskKind::Lookup, Tier::Low, Tier::Medium, Effort::High),
            ] {
                let facts = CrewmateFacts {
                    task: TaskId::new(1).unwrap(),
                    agent: None,
                    authority: Authority::CardChoice,
                    kind: Decided::Answer {
                        value: kind,
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
                };
                let tx = conn.transaction().unwrap();
                let RouteOutcome::Routed(routed) =
                    route_crewmate(&tx, &table, &config, &facts, &health, Timestamp::UNIX_EPOCH)
                        .unwrap()
                else {
                    panic!("combo must route: legacy={legacy}, kind={kind}")
                };
                tx.commit().unwrap();
                let choice = routed.record.choice.as_ref().unwrap();
                assert_eq!(routed.record.tier_landed, landed);
                assert_eq!((choice.tier, choice.effort), (used, effort));
                if landed == Tier::Low {
                    assert_eq!(routed.record.fallbacks.last().unwrap().to, Tier::Medium);
                    assert!(routed.record.why.contains("low runs on medium"));
                } else {
                    assert_eq!(routed.record.fallbacks, Vec::<FallbackStep>::new());
                }
                let decisions = routing_decisions::decisions_for_task(&conn, facts.task).unwrap();
                let (id, decision) = decisions.last().unwrap();
                assert_eq!(*id, routed.decision);
                assert_eq!(decision.tier_landed, landed);
                assert_eq!(
                    decision.choice,
                    Some(DecisionChoice {
                        tier: used,
                        model: model.canonical.clone(),
                        account: account.id,
                        effort,
                    })
                );
                assert_session_choice(&routed, &account, &model);
            }
            assert_eq!(models::list_placements(&conn).unwrap(), saved);
        }
    }

    #[derive(Debug)]
    struct Catalog;

    impl ModelCatalog for Catalog {
        fn resolve(&self, _: Provider, detected: &DetectedModel) -> ModelFacts {
            ModelFacts {
                model: detected.id.clone(),
                canonical: detected.id.clone(),
                display_name: None,
                family: "test".into(),
                generation: 1,
                image_input: false,
                context_window: 8192,
                efforts: Vec::new(),
                known: true,
            }
        }

        fn catalog_list(&self, _: Provider) -> Vec<DetectedModel> {
            Vec::new()
        }

        fn starter_set(&self, _: Provider) -> StarterTiers {
            panic!("detection must not request starter presets")
        }
    }

    #[tokio::test]
    async fn detection_leaves_empty_and_partial_tiers_unchanged() {
        let root = std::env::temp_dir().join(format!(
            "qm-detection-tiers-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = Db::open(&root.join("state.db")).unwrap();
        let account = db
            .call(|conn| {
                let id = accounts::insert_account(
                    conn,
                    &NewAccount {
                        provider: Provider::Claude,
                        label: "test".into(),
                        kind: AccountKind::Subscription,
                        credential_ref: None,
                        base_url: None,
                        at: Timestamp::UNIX_EPOCH,
                    },
                )?;
                Ok(accounts::load_account(conn, id)?.unwrap())
            })
            .await
            .unwrap();
        let clock = SystemClock;
        let engine = ScriptedEngine::new(
            Script::from_toml("name = 'tiers'\nmodels = ['test-model']").unwrap(),
            Arc::new(clock),
        );
        let first = detect_account(&db, &engine, &account, &Catalog, &clock)
            .await
            .unwrap();
        assert_eq!(first.seeded, 0);
        db.call(move |conn| {
            assert_eq!(models::list_placements(conn)?, Vec::new());
            assert!(!models::starter_seeded(conn, Provider::Claude)?);
            assert_eq!(models::list_models(conn, account.id)?.len(), 1);
            Ok(())
        })
        .await
        .unwrap();
        let saved = db
            .call(|conn| {
                models::place_model(
                    conn,
                    &ModelId::new("test-model").unwrap(),
                    None,
                    None,
                    Tier::Low,
                    Timestamp::UNIX_EPOCH,
                )
                .unwrap();
                models::list_placements(conn)
            })
            .await
            .unwrap();
        let again = detect_account(&db, &engine, &account, &Catalog, &clock)
            .await
            .unwrap();
        assert_eq!(again.seeded, 0);
        db.call(move |conn| {
            assert_eq!(models::list_placements(conn)?, saved);
            assert!(!models::starter_seeded(conn, Provider::Claude)?);
            Ok(())
        })
        .await
        .unwrap();
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "routing_premium_tests.rs"]
mod premium_tests;
