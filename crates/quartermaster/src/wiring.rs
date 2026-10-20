//! The core, wired.
//!
//! [`start_core`] builds what the core runs on and serves the UI until it quits:
//!
//! 1. **Engines.** The Claude engine when `claude` is installed, the provider engine always, both
//!    under the OS supervisor recording into the store. The scripted engine only behind the
//!    developer gate (below).
//! 2. **Detection** of every account's models, in the background: it never blocks startup.
//! 3. **The seat.** `Seat` (its session, resumed where it can be), `ContextBuilder`, `Conversation`
//!    and `CoreToolHost`, joined by the seat's inbox to the `TurnLoop`, which recovers what the
//!    last run left, opens the session and claims pending wakes first.
//! 4. **The API.** `qm_core::api::serve` answers the UI. On quit it tells the loop to shut down;
//!    this function then waits up to [`LOOP_GRACE`] for the loop, so the interrupted turn is
//!    recorded and the session closed before the runtime stops (no agent process outlives the
//!    app).
//!
//! **The developer seat.** With the developer gate open (`QM_DEV=1` and an explicit
//! `QUARTERMASTER_HOME` that isn't `~/.quartermaster`, as for `quartermaster dev`), setting
//! [`DEV_SEAT_SCRIPT`] to a script file runs the seat on the scripted engine with that script, on
//! a scripted account, instead of the routed model; nothing changes for a normal launch.

use crate::catalog::Catalog;
use qm_auth::login::LoginEndpoints;
use qm_auth::{AuthError, CredentialStore, FileCredentialStore, LoginHealth, TokenKeeper};
use qm_core::api::{self, ApiDeps, ApiPart, Conversation};
use qm_core::context_block::ContextBuilder;
use qm_core::crew_view::CrewPart;
use qm_core::decisions::cards::DecisionCards;
use qm_core::decisions::diff::{DiffPart, GitDiffReader};
use qm_core::decisions::performer::DecisionPerformer;
use qm_core::decisions::{DecisionDeps, DecisionService};
use qm_core::delivery::ports::ParentPort;
use qm_core::delivery::probe::GithubDeliveryProbe;
use qm_core::delivery::{DeliveryCloseFacts, DeliveryDeps, DeliveryService};
use qm_core::held::{HeldPart, HeldService, InboxSignal};
use qm_core::inbox::{Inbox, SeatSink, WakeSink};
use qm_core::notifications::{DesktopNotifier, NotificationsPart};
use qm_core::parent_channel::ParentChannel;
use qm_core::processes::StoreRecorder;
use qm_core::projects::{AddProjectPerformer, ProjectDeps, ProjectService, ProjectsPart};
use qm_core::reconcile::ReconcileService;
use qm_core::routing::{self, Engines, ModelCatalog};
use qm_core::scheduler::Scheduler;
use qm_core::schedules::{ScheduleDeps, ScheduleService};
use qm_core::seat::{FixedSeatModel, RoutedSeatModel, Seat, SeatDeps, SeatModel, SeatPick};
use qm_core::secondmates::{
    DeclaredDecider, SecondmateDeps, SecondmatePerformer, SecondmateService, SecondmatesPart,
};
use qm_core::sessions::SessionsPart;
use qm_core::sessions::recorder::Recorder;
use qm_core::settings_api::{SettingsDeps, SettingsPart};
use qm_core::settling::SettlingService;
use qm_core::settling::cards::{GithubWriteCards, MergeCards};
use qm_core::settling::performers::{GithubWritePerformer, MergePerformer};
use qm_core::setup_check::{SetupCheck, SetupFacts, SetupPart};
use qm_core::tasks::delivery_probe::DeliveryProbe;
use qm_core::tasks::startup::{ReconcileOutcome, catch_up, reconcile};
use qm_core::tasks::{TaskHooks, TaskService, TaskWiring, TasksPart};
use qm_core::tool_host::CoreToolHost;
use qm_core::turn_io::{SeatInboxRx, seat_inbox};
use qm_core::turns::{TurnLoop, TurnLoopDeps};
use qm_core::verification::{VerificationDeps, VerificationService};
use qm_engine::scripted::{Script, ScriptedEngine};
use qm_engine::supervisor::{OsSupervisor, Supervisor};
use qm_engine::{AccountRef, Engine};
use qm_engine_claude::detect::{ClaudeInstall, LoginState};
use qm_engine_claude::{ClaudeConfig, ClaudeEngine};
use qm_github::{GhCli, GhProgram, GithubClient};
use qm_providers::{
    BuiltinProfiles, CatalogProfiles, FileToolsFactory, ModelProfiles, ProviderEngine,
    ProviderEngineConfig,
};
use qm_store::StoreError;
use qm_store::accounts::{self, NewAccount};
use qm_store::db::Db;
use qm_tools::handler::Dispatcher;
use qm_tools::handler::ToolHost;
use qm_types::api::CoreServer;
use qm_types::clock::{Clock, SystemClock};
use qm_types::config::Config;
use qm_types::data_dir::DataDir;
use qm_types::domain::HeldKind;
use qm_types::domain::{AccountKind, Effort, EngineKind, Provider};
use qm_types::env::QUARTERMASTER_HOME;
use qm_types::ids::AgentId;
use qm_types::ids::{AccountId, ModelId};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How long the app waits for the seat's loop to record the interrupted turn and close the
/// session after you quit.
pub(crate) const LOOP_GRACE: Duration = Duration::from_secs(20);

/// Behind the developer gate: a script file the seat runs on, on the scripted engine.
pub(crate) const DEV_SEAT_SCRIPT: &str = "QM_DEV_SEAT_SCRIPT";

/// The scripted account the developer seat runs on.
const DEV_ACCOUNT_LABEL: &str = "Scripted (developer seat)";

/// Builds the core and serves the UI until it quits or goes away. If the seat can't be set up at
/// all, the UI still gets a minimal core that shows the problem, so it can quit cleanly.
#[allow(clippy::too_many_lines)] // one step per startup step; splitting it would hide the order
pub(crate) async fn start_core(
    dir: DataDir,
    db: Db,
    config: Config,
    server: CoreServer,
    machine: String,
    dev_script: Option<Script>,
) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let base = foundations(&dir, &db, config, &clock, dev_script).await;
    let seat = Arc::new(Seat::new(SeatDeps {
        db: db.clone(),
        clock: Arc::clone(&clock),
        data_dir: dir.clone(),
        engines: base.engines.clone(),
        model: Arc::clone(&base.model),
    }));
    let agent = match seat.agent().await {
        Ok(agent) => agent.id,
        Err(e) => {
            tracing::error!(error = %e, "couldn't set up the Quartermaster's row");
            let problem = format!(
                "The app couldn't start: it can't set up the Quartermaster in its database \
                 ({e}). Nothing can be changed this run; quit, and see the log in {}.",
                dir.logs().display()
            );
            crate::core_link::serve(server, machine, problem).await;
            return;
        }
    };

    let conversation = Arc::new(Conversation::new(db.clone(), Arc::clone(&clock), agent));
    let registry = registry(&db, &clock, &dir, base.config.subscribe());
    let wakes = Inbox::new();
    let gh = gh_program(registry.0.facts());
    let github = GithubClient::new(Arc::new(GhCli::following(Arc::clone(&gh))));
    let verification = verification_service(&db, &clock, &dir, &base.health, &wakes, &conversation);
    let tasks = crew_tasks(
        CrewInputs {
            dir: &dir,
            db: &db,
            clock: &clock,
            engines: base.engines.clone(),
            config: base.config.subscribe(),
            setup: registry.0.facts(),
            health: Arc::clone(&base.health),
            github: github.clone(),
        },
        wakes.clone(),
        Arc::clone(&conversation),
        agent,
        verification_hooks(&verification),
    );
    // Every crewmate's session is recorded from its first turn.
    let recorder = tasks.as_ref().map(|tasks| {
        let recorder = Recorder::spawn(db.clone(), Arc::clone(&clock), conversation.clone());
        tasks.set_session_sink(Arc::new(recorder.clone()));
        recorder
    });

    // The parent channel; settling nudges it when a secondmate's task lands.
    let parent_channel = ParentChannel::new(db.clone(), Arc::clone(&clock), wakes.clone());
    let delivery_settling = tasks.as_ref().map(|t| {
        settling_services(
            t,
            github,
            gh,
            &verification,
            Arc::new(parent_channel.clone()),
        )
    });
    // The one held-action service. Every held action's wake reaches its target's loop through
    // the wake inbox: the seat's, a crewmate's runner (a held GitHub write answers the
    // crewmate) or a secondmate's loop.
    let held = HeldService::new(
        db.clone(),
        Arc::clone(&clock),
        Arc::new(InboxSignal(wakes.clone())),
    );
    // The secondmates, after the task service and the held actions.
    let secondmates = secondmate_service(&db, &clock, &dir, &base, (&wakes, &held), &conversation);
    // Decisions (cards, stuck notices, diffs) over the task service.
    let decisions = tasks
        .as_ref()
        .map(|tasks| decision_service(&db, &clock, &wakes, &conversation, tasks));
    let host = tool_host(
        CoreToolHost::new(db.clone(), Arc::clone(&clock), Arc::clone(&conversation))
            .with_parent_channel(parent_channel.clone()),
        &registry.1,
        tasks.as_ref(),
        delivery_settling.as_ref(),
        &secondmates,
        (&verification, decisions.as_ref()),
    );
    // Schedules, on their own scheduler, behind `schedule` and `cancel_schedule`.
    let schedules = ScheduleService::new(ScheduleDeps {
        db: db.clone(),
        clock: Arc::clone(&clock),
        inbox: wakes.clone(),
        scheduler: Scheduler::spawn(Arc::clone(&clock)).0,
    });
    host.set_schedules(schedules.clone());
    let (inbox, inbox_rx) = seat_inbox();
    wakes.register(
        agent,
        &(Arc::new(SeatSink(inbox.clone())) as Arc<dyn WakeSink>),
    );
    // Reconcile the crew's turns, agents and worktrees before the seat opens.
    let reconciled = crew_reconcile(tasks.as_ref()).await;
    // Settles left half-done are concluded before the seat runs or any card
    // can be answered.
    if let Some(delivery_settling) = &delivery_settling {
        delivery_settling.settling.recover().await;
    }
    let mut seat_loop = spawn_seat_loop(&db, &clock, seat, &conversation, host, inbox_rx);
    // Every secondmate's loop, idle until work arrives.
    start_secondmates(&secondmates);
    // Resume the crew once the seat runs.
    if let Some(tasks) = &tasks {
        let crew = tasks.clone();
        tokio::spawn(async move { catch_up(&crew, reconciled).await });
    }
    spawn_settle_recovery(delivery_settling.as_ref());
    // The parent channel's pump, next to the scheduler, until quitting.
    let (pump_stop, pump_rx) = watch::channel(false);
    tokio::spawn(parent_channel.run(pump_rx));
    // Schedules missed while closed fire now, marked late.
    tokio::spawn(async move {
        if let Err(e) = schedules.start().await {
            tracing::error!(error = %e, "couldn't arm the pending schedules");
        }
    });
    let notify_config = base.config.subscribe();

    let mut parts = settings_parts(&db, &clock, &dir, base, &conversation);
    parts.extend(registry_parts(
        &held,
        &conversation,
        registry,
        delivery_settling.as_ref(),
        decisions.as_ref(),
    ));
    if let Some(tasks) = &tasks {
        parts.push(Arc::new(TasksPart::new(tasks.clone())));

        parts.push(Arc::new(
            SessionsPart::new(
                db.clone(),
                Arc::clone(&clock),
                ReconcileService::new(tasks.clone()),
                recorder,
            )
            .with_secondmates(secondmates.clone()),
        ));
    }
    parts.push(Arc::new(SecondmatesPart::new(secondmates.clone())));
    // Last, so it sees every card the other parts add.
    parts.push(Arc::new(NotificationsPart::new(
        notify_config,
        Arc::new(DesktopNotifier),
    )));
    api::serve(
        server,
        ApiDeps {
            inbox,
            conversation,
            machine,
            parts,
        },
    )
    .await;

    // The crew stops too when the UI went away without quitting (idempotent after a quit).
    if let Some(tasks) = &tasks {
        tasks.shutdown().await;
    }
    secondmates.shutdown().await;
    pump_stop.send_replace(true);
    // `serve` has told the loop to shut down (and dropped its inbox).
    if tokio::time::timeout(LOOP_GRACE, &mut seat_loop)
        .await
        .is_err()
    {
        tracing::warn!(
            "the Quartermaster's turn loop didn't stop within {} seconds",
            LOOP_GRACE.as_secs()
        );
        seat_loop.abort();
    }
}

/// What everything else is built on: the engines, the logins, the catalog, the settings channel
/// and the seat's model.
struct Foundations {
    engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    tokens: Option<Arc<TokenKeeper>>,
    catalog: Option<Arc<dyn ModelCatalog>>,
    /// Settings reach routing and the seat's model through this channel; the settings part holds
    /// the sender.
    config: watch::Sender<Config>,
    /// Which logins work, shared by the seat's model, the token keeper and the crew.
    health: Arc<LoginHealth>,
    model: Arc<dyn SeatModel>,
    /// Why the settings part can't run, when `tokens` or `catalog` is missing.
    settings_problem: Option<String>,
}

/// The engines (with detection in the background), the logins and the seat's model.
async fn foundations(
    dir: &DataDir,
    db: &Db,
    config: Config,
    clock: &Arc<dyn Clock>,
    dev_script: Option<Script>,
) -> Foundations {
    let (credentials, credentials_problem) = match open_credentials(dir) {
        Ok(store) => (Some(store), None),
        Err(problem) => (None, Some(problem)),
    };
    let (catalog, catalog_problem) = match load_catalog() {
        Ok(catalog) => (Some(catalog), None),
        Err(problem) => (None, Some(problem)),
    };
    // One record of which logins work, and one token keeper over the shared credential store: the
    // provider engine refreshes through the keeper, which records broken logins, and routing leaves
    // those accounts out.
    let health = Arc::new(LoginHealth::new());
    let tokens = token_keeper(credentials.as_ref(), &health, &LoginEndpoints::default());
    let (engines, claude_plan) = engines(db, clock, tokens.clone(), dev_script.clone()).await;
    // A Claude subscription isn't logged into in the app; its account follows the
    // `claude` login, so it is there (and its models read below) from the first launch.
    if let Some(plan) = claude_plan {
        match qm_core::settings_api::follow_claude_login(db, clock.as_ref(), Some(plan)).await {
            Ok(row) => {
                tracing::info!(account = %row.id, "the Claude account follows your claude login");
            }
            Err(e) => {
                tracing::error!(error = %e, "couldn't add the Claude account for your claude login");
            }
        }
    }
    let detected = match &catalog {
        Some(catalog) => spawn_detection(db, &engines, Arc::clone(catalog), clock),
        None => watch::channel(true).1,
    };
    let (config, config_rx) = watch::channel(config);
    let model = seat_model(
        db,
        clock,
        dev_script.is_some(),
        config_rx,
        &health,
        detected,
    )
    .await;
    Foundations {
        engines,
        tokens,
        catalog,
        config,
        health,
        model,
        settings_problem: credentials_problem.or(catalog_problem),
    }
}

/// The core's tool host (`base`, with the parent channel), with the project registry and the
/// task service behind their tools.
fn tool_host(
    base: CoreToolHost,
    projects: &ProjectService,
    tasks: Option<&TaskService>,
    settling: Option<&Settling>,
    secondmates: &SecondmateService,
    (verification, decisions): (&VerificationService, Option<&DecisionService>),
) -> Arc<CoreToolHost> {
    let mut host = base
        .with_projects(projects.clone())
        .with_secondmates(secondmates.clone())
        .with_verification(verification.clone());
    if let Some(decisions) = decisions {
        host = host.with_decisions(decisions.clone());
    }
    if let Some(tasks) = tasks {
        host = host.with_tasks(tasks.clone());
    }
    if let Some(delivery_settling) = settling {
        host = host
            .with_delivery(delivery_settling.delivery.clone())
            .with_settling(delivery_settling.settling.clone());
    }
    let host = Arc::new(host);
    let dyn_host: Arc<dyn ToolHost> = host.clone();
    if let Some(tasks) = tasks {
        tasks.set_tool_host(Arc::downgrade(&dyn_host));
    }
    secondmates.set_tool_host(Arc::downgrade(&dyn_host));
    host
}

/// The secondmates, with their performer registered for held proposals.
fn secondmate_service(
    db: &Db,
    clock: &Arc<dyn Clock>,
    dir: &DataDir,
    base: &Foundations,
    (wakes, held): (&Inbox, &HeldService),
    conversation: &Arc<Conversation>,
) -> SecondmateService {
    let secondmates = SecondmateService::new(SecondmateDeps {
        db: db.clone(),
        clock: Arc::clone(clock),
        data_dir: dir.clone(),
        engines: base.engines.clone(),
        model: Arc::clone(&base.model),
        decider: Arc::new(DeclaredDecider),
        inbox: wakes.clone(),
        held: held.clone(),
        dispatcher: Dispatcher::builtin(),
        ui: Arc::clone(conversation),
    });
    held.register(
        HeldKind::Secondmate,
        Arc::new(SecondmatePerformer::new(secondmates.clone())),
    );
    secondmates
}

/// Every secondmate's loop, in the background.
fn start_secondmates(secondmates: &SecondmateService) {
    let secondmates = secondmates.clone();
    tokio::spawn(async move {
        if let Err(e) = secondmates.start_all().await {
            tracing::error!(error = %e, "couldn't start the secondmates");
        }
    });
}

/// Startup reconciliation for the crew; nothing is deleted.
async fn crew_reconcile(tasks: Option<&TaskService>) -> ReconcileOutcome {
    let Some(tasks) = tasks else {
        return ReconcileOutcome::default();
    };
    reconcile(tasks).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "the crew's startup check failed");
        ReconcileOutcome::default()
    })
}

/// The seat's turn loop, on its own task.
fn spawn_seat_loop(
    db: &Db,
    clock: &Arc<dyn Clock>,
    seat: Arc<Seat>,
    conversation: &Arc<Conversation>,
    host: Arc<CoreToolHost>,
    inbox_rx: SeatInboxRx,
) -> tokio::task::JoinHandle<()> {
    let turn_loop = TurnLoop::new(
        TurnLoopDeps {
            db: db.clone(),
            clock: Arc::clone(clock),
            seat,
            context: Arc::new(ContextBuilder::new(db.clone(), Arc::clone(clock))),
            chat: conversation.clone(),
            host,
            dispatcher: Dispatcher::builtin(),
        },
        inbox_rx,
    );
    tokio::spawn(async move {
        if let Err(e) = turn_loop.run().await {
            tracing::error!(error = %e, "the Quartermaster's turn loop stopped");
        }
    })
}

/// The crew pane's part and the settings screens' part.
fn settings_parts(
    db: &Db,
    clock: &Arc<dyn Clock>,
    dir: &DataDir,
    base: Foundations,
    conversation: &Arc<Conversation>,
) -> Vec<Arc<dyn ApiPart>> {
    let mut parts: Vec<Arc<dyn ApiPart>> =
        vec![Arc::new(CrewPart::new(db.clone(), Arc::clone(clock)))];
    if let (Some(tokens), Some(catalog)) = (base.tokens, base.catalog) {
        parts.push(Arc::new(SettingsPart::new(SettingsDeps {
            db: db.clone(),
            clock: Arc::clone(clock),
            data_dir: dir.clone(),
            config: base.config,
            tokens,
            engines: base.engines,
            catalog,
            conversation: Arc::clone(conversation),
            endpoints: LoginEndpoints::default(),
        })));
    } else {
        let problem = base.settings_problem.unwrap_or_else(|| {
            "Settings can't change anything this run: the logins aren't available.".to_owned()
        });
        tracing::error!("{problem}");
        parts.push(Arc::new(UnavailableSettings(problem)));
    }
    parts
}

/// Stands in for the settings part when it can't run: Settings shows why instead of staying
/// empty with no reason.
#[derive(Debug)]
struct UnavailableSettings(String);

impl ApiPart for UnavailableSettings {
    fn handle<'a>(
        &'a self,
        _command: &'a qm_types::api::Command,
    ) -> Option<qm_types::BoxFuture<'a, api::Handled>> {
        None
    }

    fn fill<'a>(
        &'a self,
        snapshot: &'a mut qm_types::api::Snapshot,
    ) -> qm_types::BoxFuture<'a, ()> {
        Box::pin(async move {
            snapshot.settings.unavailable = Some(self.0.clone());
        })
    }
}

/// One credential store for every reader and writer of auth.json; why not, in a sentence.
fn open_credentials(dir: &DataDir) -> Result<Arc<dyn CredentialStore>, String> {
    match FileCredentialStore::open(dir.auth_file()) {
        Ok(store) => Ok(Arc::new(store)),
        Err(e) => {
            tracing::error!(error = %e, "can't read auth.json; the provider engine is off");
            // A damaged file's reason already names it; the others don't.
            let reason = match &e {
                AuthError::Corrupt(_) => e.to_string(),
                _ => format!("{} can't be read ({e})", dir.auth_file().display()),
            };
            Err(format!(
                "Settings can't connect or change accounts this run: {reason}. \
                 Nothing was changed; fix or move that file and restart the app."
            ))
        }
    }
}

/// The one token keeper over the shared credential store, recording into `health`.
fn token_keeper(
    credentials: Option<&Arc<dyn CredentialStore>>,
    health: &Arc<LoginHealth>,
    endpoints: &LoginEndpoints,
) -> Option<Arc<TokenKeeper>> {
    let store = Arc::clone(credentials?);
    Some(Arc::new(TokenKeeper::new(
        store,
        endpoints.clone(),
        Arc::clone(health),
    )))
}

/// The built-in model catalog, adapted for routing; why not, in a sentence.
fn load_catalog() -> Result<Arc<dyn ModelCatalog>, String> {
    match qm_providers::catalog::Catalog::embedded() {
        Ok(catalog) => Ok(Arc::new(Catalog(catalog))),
        Err(e) => {
            tracing::error!(error = %e, "the built-in model catalog is unreadable; detection is off");
            Err(format!(
                "Settings can't change anything this run: the built-in model catalog can't be \
                 read ({e})."
            ))
        }
    }
}

/// Where the seat's model comes from: routing, or the developer seat's fixed pick.
async fn seat_model(
    db: &Db,
    clock: &Arc<dyn Clock>,
    dev_seat: bool,
    config: watch::Receiver<Config>,
    health: &Arc<LoginHealth>,
    detected: watch::Receiver<bool>,
) -> Arc<dyn SeatModel> {
    if dev_seat {
        match dev_pick(db, clock).await {
            Ok(pick) => return Arc::new(FixedSeatModel(pick)),
            Err(e) => tracing::error!(error = %e, "couldn't set up the developer seat's account"),
        }
    }
    Arc::new(
        RoutedSeatModel::new(db.clone(), Arc::clone(clock), config, Arc::clone(health))
            .with_detection(detected),
    )
}

/// What the task service reads from the rest of the core.
struct CrewInputs<'a> {
    dir: &'a DataDir,
    db: &'a Db,
    clock: &'a Arc<dyn Clock>,
    engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    config: watch::Receiver<Config>,
    /// The setup check's facts, for the delivery probe.
    setup: watch::Receiver<SetupFacts>,
    health: Arc<LoginHealth>,
    /// GitHub's API with the token from `gh auth token`, for the delivery probe.
    github: GithubClient,
}

/// The task service: crewmates, the wake inbox's producers and the scheduler. `None` (and the task
/// tools unavailable) only if the shipped routing rules can't be read.
fn crew_tasks(
    inputs: CrewInputs<'_>,
    wakes: Inbox,
    chat: Arc<Conversation>,
    seat_agent: AgentId,
    hooks: TaskHooks,
) -> Option<TaskService> {
    let CrewInputs {
        dir,
        db,
        clock,
        engines,
        config,
        setup,
        health,
        github,
    } = inputs;
    let probe: Arc<dyn DeliveryProbe> =
        Arc::new(GithubDeliveryProbe::for_app(setup.clone(), github));
    let recorder = StoreRecorder::new(db.clone(), Arc::clone(clock));
    let supervisor: Arc<dyn Supervisor> = Arc::new(OsSupervisor::new(Some(Arc::new(recorder))));
    let (scheduler, _scheduler_task) = Scheduler::spawn(Arc::clone(clock));
    let home = std::env::var_os("HOME").map_or_else(|| dir.root().to_owned(), PathBuf::from);
    match TaskService::wire_with(
        TaskWiring {
            db: db.clone(),
            clock: Arc::clone(clock),
            data_dir: dir.clone(),
            home,
            engines,
            supervisor,
            inbox: wakes,
            scheduler,
            config,
            health,
            chat,
            seat_agent,
            setup,
            probe: Some(probe),
        },
        hooks,
    ) {
        Ok(tasks) => Some(tasks),
        Err(e) => {
            tracing::error!(error = %e, "the routing rules are unreadable; crewmates are off");
            None
        }
    }
}

/// The engines this Mac can run, by kind, and the subscription type `claude` is logged in with,
/// if it is.
async fn engines(
    db: &Db,
    clock: &Arc<dyn Clock>,
    tokens: Option<Arc<TokenKeeper>>,
    dev_script: Option<Script>,
) -> (BTreeMap<EngineKind, Arc<dyn Engine>>, Option<String>) {
    let recorder = StoreRecorder::new(db.clone(), Arc::clone(clock));
    let supervisor: Arc<dyn Supervisor> = Arc::new(OsSupervisor::new(Some(Arc::new(recorder))));
    let inherited: BTreeMap<String, String> = std::env::vars().collect();
    let mut engines: BTreeMap<EngineKind, Arc<dyn Engine>> = BTreeMap::new();

    // Finding `claude` runs it (`--version`, `auth status`): off the async threads. Only a
    // claude.ai subscription login turns the Claude engine on (never an API key).
    let env = inherited.clone();
    let install = tokio::task::spawn_blocking(move || qm_engine_claude::detect::detect(&env))
        .await
        .ok();
    let (claude, plan) = match install {
        Some(ClaudeInstall {
            path: Some(path),
            login: LoginState::Subscription { subscription_type },
            ..
        }) => (Some(path), Some(subscription_type)),
        Some(install) => {
            if let Some(reason) = install.unavailable_reason() {
                tracing::info!("{reason}");
            }
            (None, None)
        }
        None => (None, None),
    };
    if let Some(claude) = claude {
        engines.insert(
            EngineKind::Claude,
            Arc::new(ClaudeEngine::new(ClaudeConfig {
                claude,
                supervisor: Arc::clone(&supervisor),
                clock: Arc::clone(clock),
                inherited_env: inherited.clone(),
            })),
        );
    }

    if let Some(tokens) = tokens {
        {
            engines.insert(
                EngineKind::Provider,
                Arc::new(ProviderEngine::new(ProviderEngineConfig {
                    tokens,
                    supervisor: Arc::clone(&supervisor),
                    clock: Arc::clone(clock),
                    models: provider_models(),
                    builtins: Some(Arc::new(FileToolsFactory)),
                    inherited_env: inherited,
                    user_agent: format!("quartermaster/{}", env!("CARGO_PKG_VERSION")),
                })),
            );
        }
    }

    if let Some(script) = dev_script {
        engines.insert(
            EngineKind::Scripted,
            Arc::new(ScriptedEngine::new(script, Arc::clone(clock))),
        );
    }
    (engines, plan)
}

/// What the provider engine knows about each model: the built-in catalog's profiles, or the
/// built-in ChatGPT list if the catalog can't be read (startup has already said so).
pub(crate) fn provider_models() -> Arc<dyn ModelProfiles> {
    match qm_providers::catalog::Catalog::embedded() {
        Ok(catalog) => Arc::new(CatalogProfiles(catalog)),
        Err(_) => Arc::new(BuiltinProfiles),
    }
}

/// Reads every account's model list in the background (never blocks startup). The
/// receiver turns `true` when that pass has finished, however it ended.
fn spawn_detection(
    db: &Db,
    engines: &BTreeMap<EngineKind, Arc<dyn Engine>>,
    catalog: Arc<dyn ModelCatalog>,
    clock: &Arc<dyn Clock>,
) -> watch::Receiver<bool> {
    let (done, detected) = watch::channel(false);
    let engines: Engines = engines
        .iter()
        .map(|(kind, engine)| (*kind, Arc::clone(engine)))
        .collect::<HashMap<_, _>>();
    let detection = routing::spawn_detection(db.clone(), engines, catalog, Arc::clone(clock));
    tokio::spawn(async move {
        match detection.await {
            Ok(Ok(outcomes)) => {
                for o in outcomes {
                    if let Some(error) = &o.read_error {
                        tracing::warn!(account = %o.account, source = ?o.source, %error, "model list not read");
                    } else {
                        tracing::info!(account = %o.account, source = ?o.source, "model list read");
                    }
                }
            }
            Ok(Err(e)) => tracing::error!(error = %e, "model detection failed"),
            Err(e) => tracing::error!(error = %e, "model detection panicked"),
        }
        done.send_replace(true);
    });
    detected
}

/// The developer seat's script, when the gate is open and [`DEV_SEAT_SCRIPT`] names one. A
/// script that is named but can't be loaded is an error the app stops on: running on with the
/// routed model instead would spend a real account's turns on a test.
pub(crate) fn dev_seat_script(dir: &DataDir) -> Result<Option<Script>, String> {
    let Some(path) = std::env::var_os(DEV_SEAT_SCRIPT).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if !dev_gate_open(dir.root()) {
        tracing::warn!(
            "{DEV_SEAT_SCRIPT} is set but the developer gate is closed (QM_DEV=1 and a throwaway \
             QUARTERMASTER_HOME); it is ignored"
        );
        return Ok(None);
    }
    let path = PathBuf::from(path);
    Script::load(&path)
        .map(Some)
        .map_err(|e| format!("can't load the seat script {}: {e}", path.display()))
}

/// The developer gate of `crates/quartermaster/src/dev/mod.rs`: `QM_DEV=1` and an explicit
/// `QUARTERMASTER_HOME` that isn't the real `~/.quartermaster`.
fn dev_gate_open(root: &Path) -> bool {
    if std::env::var("QM_DEV").as_deref() != Ok("1")
        || std::env::var_os(QUARTERMASTER_HOME).is_none_or(|h| h.is_empty())
    {
        return false;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return true;
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    canon(root) != canon(&home.join(".quartermaster"))
}

/// The developer seat's pick: the scripted engine on a scripted account (created once).
async fn dev_pick(db: &Db, clock: &Arc<dyn Clock>) -> Result<SeatPick, StoreError> {
    let at = clock.now();
    let id: AccountId = db
        .call(move |conn| {
            if let Some(row) = accounts::list_active(conn)?
                .into_iter()
                .find(|a| a.provider == Provider::Scripted)
            {
                return Ok(row.id);
            }
            accounts::insert_account(
                conn,
                &NewAccount {
                    provider: Provider::Scripted,
                    label: DEV_ACCOUNT_LABEL.to_owned(),
                    kind: AccountKind::Subscription,
                    credential_ref: None,
                    base_url: None,
                    at,
                },
            )
        })
        .await?;
    Ok(SeatPick {
        engine: EngineKind::Scripted,
        account: AccountRef {
            id,
            provider: Provider::Scripted,
            credential: None,
            base_url: None,
        },
        model: ModelId::new("scripted").map_err(|e| StoreError::Invalid {
            column: "agents.model",
            message: e.to_string(),
        })?,
        effort: Effort::Low,
    })
}

/// The setup check and the project registry, both reading the app's own environment.
fn registry(
    db: &Db,
    clock: &Arc<dyn Clock>,
    dir: &DataDir,
    config: watch::Receiver<Config>,
) -> (SetupCheck, ProjectService) {
    let app_env: BTreeMap<String, String> = std::env::vars().collect();
    let setup = SetupCheck::new(db.clone(), Arc::clone(clock), app_env.clone());
    let projects = ProjectService::new(ProjectDeps {
        db: db.clone(),
        clock: Arc::clone(clock),
        data_dir: dir.clone(),
        config,
        setup: setup.facts(),
        env: app_env,
    });
    (setup, projects)
}

/// The registry's API parts: held actions with the add-project performer, the projects pane, and
/// the setup check, whose first run and the personal workspace start in the background.
fn registry_parts(
    held: &HeldService,
    conversation: &Arc<Conversation>,
    (setup, projects): (SetupCheck, ProjectService),
    settling: Option<&Settling>,
    decisions: Option<&DecisionService>,
) -> Vec<Arc<dyn ApiPart>> {
    held.register(
        HeldKind::AddProject,
        Arc::new(AddProjectPerformer::new(projects.clone())),
    );
    // Merges, pushes and GitHub writes, with their cards.
    if let Some(delivery_settling) = settling {
        let merges = Arc::new(MergePerformer::new(delivery_settling.settling.clone()));
        held.register(HeldKind::Merge, merges.clone());
        held.register(HeldKind::Push, merges);
        held.register(
            HeldKind::GithubWrite,
            Arc::new(GithubWritePerformer::new(
                delivery_settling.delivery.clone(),
            )),
        );
        held.register_card(HeldKind::Merge, Arc::new(MergeCards));
        held.register_card(HeldKind::Push, Arc::new(MergeCards));
        held.register_card(HeldKind::GithubWrite, Arc::new(GithubWriteCards));
    }
    // Decision cards and stuck notices, carried out exactly.
    if let Some(decisions) = decisions {
        held.register(
            HeldKind::Decision,
            Arc::new(DecisionPerformer::new(decisions.clone())),
        );
        held.register_card(HeldKind::Decision, Arc::new(DecisionCards));
    }
    let setup_part = SetupPart::new(setup, Arc::clone(conversation));
    setup_part.start();
    let projects_part = ProjectsPart::new(projects, held.clone());
    projects_part.start(Arc::clone(conversation));
    let mut parts: Vec<Arc<dyn ApiPart>> = vec![
        Arc::new(HeldPart::new(held.clone(), Arc::clone(conversation))),
        Arc::new(projects_part),
        Arc::new(setup_part),
    ];
    if let Some(decisions) = decisions {
        parts.push(Arc::new(DiffPart::new(decisions.clone())));
    }
    parts
}

/// Delivery (pull requests, checks, the GitHub tools) and settling.
#[derive(Clone, Debug)]
struct Settling {
    delivery: DeliveryService,
    settling: SettlingService,
}

/// The `gh` the app runs for GitHub's API token and its pushes: the one the setup check has found
/// now, else `gh` on `PATH`. Read at each use, so a `gh` installed from the setup check after
/// startup is used without a restart.
fn gh_program(facts: watch::Receiver<SetupFacts>) -> GhProgram {
    Arc::new(move || {
        facts
            .borrow()
            .paths
            .gh
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"))
    })
}

/// At startup: the CI poller is re-armed for every pushed head still pending (the
/// settles the last run left half-done were concluded before the seat started).
fn spawn_settle_recovery(settling: Option<&Settling>) {
    let Some(delivery_settling) = settling else {
        return;
    };
    let delivery = delivery_settling.delivery.clone();
    tokio::spawn(async move {
        if let Err(e) = delivery.recover().await {
            tracing::error!(error = %e, "couldn't re-arm the CI poller");
        }
    });
}

/// Delivery and settling over the task service, with verification behind delivery and delivery's
/// checks behind verification, the tasks' close hook, and the parent port that tells a secondmate
/// when its crewmate's task lands.
fn settling_services(
    tasks: &TaskService,
    github: GithubClient,
    gh: GhProgram,
    verification: &VerificationService,
    parent: Arc<dyn ParentPort>,
) -> Settling {
    let delivery = DeliveryService::new(DeliveryDeps {
        tasks: tasks.clone(),
        github,
        gh,
        verification: Arc::new(verification.clone()),
        parent,
    });
    verification.set_ci(Arc::new(delivery.clone()));
    tasks.set_close_facts(Arc::new(DeliveryCloseFacts::new(delivery.clone())));
    Settling {
        settling: SettlingService::new(delivery.clone()),
        delivery,
    }
}

/// Verification, built before the task service (it is its report gate, start hook and stuck
/// choices).
fn verification_service(
    db: &Db,
    clock: &Arc<dyn Clock>,
    dir: &DataDir,
    health: &Arc<LoginHealth>,
    wakes: &Inbox,
    conversation: &Arc<Conversation>,
) -> VerificationService {
    VerificationService::new(VerificationDeps {
        db: db.clone(),
        clock: Arc::clone(clock),
        data_dir: dir.clone(),
        health: Arc::clone(health),
        inbox: wakes.clone(),
        chat: conversation.clone(),
    })
}

/// The task service's hooks, all three verification's.
fn verification_hooks(verification: &VerificationService) -> TaskHooks {
    TaskHooks {
        gate: Arc::new(verification.clone()),
        start_hook: Arc::new(verification.clone()),
        choices: Arc::new(verification.clone()),
    }
}

/// Decision cards and the Diff screen, over the task service.
fn decision_service(
    db: &Db,
    clock: &Arc<dyn Clock>,
    wakes: &Inbox,
    conversation: &Arc<Conversation>,
    tasks: &TaskService,
) -> DecisionService {
    DecisionService::new(DecisionDeps {
        db: db.clone(),
        clock: Arc::clone(clock),
        inbox: wakes.clone(),
        chat: conversation.clone(),
        tasks: tasks.clone(),
        diff: Arc::new(GitDiffReader::default()),
    })
}
