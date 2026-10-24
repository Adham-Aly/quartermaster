//! Secondmates.
//!
//! A secondmate is the Quartermaster's design one level down: the same turn loop
//! ([`crate::turns::TurnLoop::for_secondmate`]) with its own session ([`session`]), context block
//! ([`context`]), conversation and wake inbox, and the secondmate's tool set. This module keeps the
//! loops and the services the Quartermaster's tools reach:
//!
//! - [`charter`]: proposing a secondmate (always a held action) and changing its charter (the
//!   standing branch only on your turns), and the performer that creates it on your yes.
//! - [`close`]: retiring a secondmate that has no open tasks, and stopping its loop.
//! - [`route`]: routing a request into its scope, with the app's marker and a fresh correlation
//!   ID.
//! - [`scope`]: the scope as code and the requests a secondmate may serve.
//! - [`api`]: your messages and `Esc` to a secondmate, and the quit.
//!
//! A secondmate is idle by default: its loop runs from startup (recovering what the last run
//! left), but its session opens only when its first turn starts, and closes again after
//! [`session::SECONDMATE_IDLE_SHUTDOWN`] of quiet.

pub mod api;
pub mod charter;
pub mod close;
pub mod context;
pub mod route;
pub mod scope;
pub mod session;

pub use api::SecondmatesPart;
pub use charter::SecondmatePerformer;
/// The MVP decider the app wires.
pub use qm_routing::decider::DeclaredDecider;

use crate::api::Conversation;
use crate::held::HeldService;
use crate::inbox::{Inbox, SeatSink, WakeSink};
use crate::seat::SeatModel;
use crate::turn_io::{SeatInbox, SeatInput, SeatStatus, seat_inbox};
use crate::turns::{TurnLoop, TurnLoopDeps};
use context::SecondmateContext;
use qm_engine::Engine;
use qm_git::Git;
use qm_routing::decider::Decider;
use qm_store::StoreError;
use qm_store::db::Db;
use qm_tools::handler::{Dispatcher, ToolFailure, ToolHost};
use qm_types::clock::Clock;
use qm_types::data_dir::DataDir;
use qm_types::domain::EngineKind;
use qm_types::ids::{AgentId, MessageId, SecondmateId};
use session::SecondmateSeat;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long quitting waits for every secondmate's loop, in all.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// `events.kind` values this module writes (`secondmate.` events show under activity).
pub mod event_kind {
    /// The Quartermaster proposed a secondmate (a held card).
    pub const PROPOSED: &str = "secondmate.proposed";
    /// You approved the proposal; the secondmate exists.
    pub const CREATED: &str = "secondmate.created";
    /// Its charter changed.
    pub const CHARTER_CHANGED: &str = "secondmate.charter_changed";
    /// A request was routed into its scope.
    pub const ROUTED: &str = "secondmate.routed";
    /// It was retired with `close_secondmate`.
    pub const RETIRED: &str = "secondmate.retired";
}

/// What the secondmates need.
pub struct SecondmateDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    /// Live settings shared with routing and both orchestrator contexts.
    pub config: tokio::sync::watch::Receiver<qm_types::config::Config>,
    pub data_dir: DataDir,
    /// Every engine the binary injected, by kind.
    pub engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    /// The Quartermaster's model: secondmates follow the same setting.
    pub model: Arc<dyn SeatModel>,
    /// Whether a proposal warrants a secondmate.
    pub decider: Arc<dyn Decider>,
    /// The wake inbox each secondmate's loop registers in.
    pub inbox: Inbox,
    pub held: HeldService,
    /// `Dispatcher::builtin()` in the app.
    pub dispatcher: Dispatcher,
    /// The Quartermaster's conversation: each secondmate's is its sibling, so a change to it
    /// re-renders an open session view.
    pub ui: Arc<Conversation>,
    /// Passive context readings for the UI; its tap goes on every session this opens.
    pub monitor: Arc<crate::context_monitor::ContextMonitor>,
}

impl fmt::Debug for SecondmateDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondmateDeps").finish_non_exhaustive()
    }
}

/// One secondmate's running loop.
struct LoopHandle {
    agent: AgentId,
    inbox: SeatInbox,
    join: Option<JoinHandle<()>>,
}

struct Inner {
    deps: SecondmateDeps,
    git: Git,
    /// The core's tool host, set once it exists (it holds this service too).
    host: OnceLock<Weak<dyn ToolHost>>,
    loops: Mutex<BTreeMap<SecondmateId, LoopHandle>>,
    shutting_down: std::sync::atomic::AtomicBool,
}

/// The secondmates: their services and their loops.
#[derive(Clone)]
pub struct SecondmateService {
    inner: Arc<Inner>,
}

impl fmt::Debug for SecondmateService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let running: Vec<SecondmateId> = self.loops().keys().copied().collect();
        f.debug_struct("SecondmateService")
            .field("loops", &running)
            .finish_non_exhaustive()
    }
}

impl SecondmateService {
    pub fn new(deps: SecondmateDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                git: Git::new(),
                host: OnceLock::new(),
                loops: Mutex::new(BTreeMap::new()),
                shutting_down: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Gives the loops the core's tool host (weak: the host holds this service).
    pub fn set_tool_host(&self, host: Weak<dyn ToolHost>) {
        let _ = self.inner.host.set(host);
    }

    pub(crate) fn deps(&self) -> &SecondmateDeps {
        &self.inner.deps
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.deps.db
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        &*self.inner.deps.clock
    }

    pub(crate) fn git(&self) -> &Git {
        &self.inner.git
    }

    fn loops(&self) -> std::sync::MutexGuard<'_, BTreeMap<SecondmateId, LoopHandle>> {
        self.inner
            .loops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn is_shutting_down(&self) -> bool {
        self.inner
            .shutting_down
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// At startup: a loop for every active secondmate. Each recovers its own turns and stays
    /// idle until a wake or your message arrives.
    pub async fn start_all(&self) -> Result<(), StoreError> {
        let ids: Vec<SecondmateId> = self
            .db()
            .call(|conn| qm_store::secondmates::active(conn))
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect();
        for id in ids {
            self.spawn(id).await;
        }
        Ok(())
    }

    /// Starts secondmate `id`'s loop if none runs (idempotent).
    pub async fn spawn(&self, id: SecondmateId) {
        if self.is_shutting_down() || self.loops().contains_key(&id) {
            return;
        }
        let Some(host) = self.inner.host.get().and_then(Weak::upgrade) else {
            tracing::error!(%id, "the tool host isn't set; the secondmate can't run");
            return;
        };
        let agent = match self
            .db()
            .call(move |conn| qm_store::secondmates::agent_of(conn, id))
            .await
        {
            Ok(Some(agent)) => agent,
            Ok(None) => {
                tracing::warn!(%id, "no agent row for the secondmate; its loop isn't started");
                return;
            }
            Err(e) => {
                tracing::error!(%id, error = %e, "couldn't read the secondmate's agent row");
                return;
            }
        };
        let deps = &self.inner.deps;
        let conversation = Arc::new(deps.ui.sibling(agent));
        let (inbox, rx) = seat_inbox();
        let turn_loop = TurnLoop::for_secondmate(
            TurnLoopDeps {
                db: deps.db.clone(),
                inbox: deps.inbox.clone(),
                clock: Arc::clone(&deps.clock),
                seat: Arc::new(SecondmateSeat::new(self.clone(), id)),
                context: Arc::new(
                    SecondmateContext::new(deps.db.clone(), &deps.clock, id)
                        .with_config(deps.config.clone()),
                ),
                chat: conversation,
                host,
                dispatcher: deps.dispatcher,
            },
            rx,
            id,
        );
        {
            let mut loops = self.loops();
            if loops.contains_key(&id) {
                return;
            }
            let join = tokio::spawn(async move {
                if let Err(e) = turn_loop.run().await {
                    tracing::error!(%id, error = %e, "a secondmate's turn loop stopped");
                }
            });
            loops.insert(
                id,
                LoopHandle {
                    agent,
                    inbox: inbox.clone(),
                    join: Some(join),
                },
            );
        }
        // Registering signals once, so wakes posted while nobody listened are caught up.
        deps.inbox
            .register(agent, &(Arc::new(SeatSink(inbox)) as Arc<dyn WakeSink>));
    }

    /// On quit: `Shutdown` to every loop, then up to [`SHUTDOWN_GRACE`] in all.
    pub async fn shutdown(&self) {
        self.inner
            .shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let joins: Vec<(SecondmateId, JoinHandle<()>)> = {
            let mut loops = self.loops();
            loops
                .iter_mut()
                .filter_map(|(id, l)| {
                    self.inner.deps.inbox.unregister(l.agent);
                    let _ = l.inbox.send(SeatInput::Shutdown);
                    l.join.take().map(|j| (*id, j))
                })
                .collect()
        };
        let clock = Arc::clone(&self.inner.deps.clock);
        let deadline = clock.instant() + SHUTDOWN_GRACE;
        for (id, mut join) in joins {
            tokio::select! {
                _ = &mut join => {}
                () = clock.sleep_until(deadline) => {
                    tracing::warn!(%id, "a secondmate's loop didn't stop in time while quitting");
                    join.abort();
                }
            }
        }
    }

    /// Stops secondmate `id`'s loop for good (it was retired): no more wakes, then `Retire`.
    /// The loop finishes on its own; nothing waits for it.
    pub(crate) fn stop_loop(&self, id: SecondmateId) {
        let Some(l) = self.loops().remove(&id) else {
            return;
        };
        self.inner.deps.inbox.unregister(l.agent);
        let _ = l.inbox.send(SeatInput::Retire);
    }

    /// Whether secondmate `id`'s loop runs, so a message to it can be delivered.
    pub(crate) fn is_running(&self, id: SecondmateId) -> bool {
        self.loops().contains_key(&id)
    }

    /// Your message, already stored on the secondmate's agent, handed to its loop as a turn you
    /// started. Called by `reconcile::intervene_secondmate` after its transaction commits.
    pub(crate) fn deliver_user_message(&self, id: SecondmateId, message: MessageId) {
        let Some(inbox) = self.loops().get(&id).map(|l| l.inbox.clone()) else {
            tracing::warn!(%id, "the secondmate stopped; your message waits for its next turn");
            return;
        };
        if inbox.send(SeatInput::User { message }).is_err() {
            tracing::warn!(%id, "the secondmate's loop has stopped; your message waits");
        }
    }

    /// `Esc` on a secondmate: stops its running turn only.
    pub fn interrupt(&self, id: SecondmateId) {
        if let Some(l) = self.loops().get(&id) {
            let _ = l.inbox.send(SeatInput::Interrupt);
        }
    }

    /// Secondmate `id`'s loop state (its running turn, your queued messages), while it runs.
    pub(crate) fn loop_status(&self, id: SecondmateId) -> Option<SeatStatus> {
        self.loops().get(&id).map(|l| l.inbox.status())
    }

    /// Whether a turn is running, for the crew row and the quit question.
    pub fn is_working(&self, id: SecondmateId) -> bool {
        self.loops()
            .get(&id)
            .is_some_and(|l| l.inbox.status().turn_running)
    }

    /// How many secondmates have a turn running.
    pub fn working_count(&self) -> u32 {
        let n = self
            .loops()
            .values()
            .filter(|l| l.inbox.status().turn_running)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

/// A store error as a tool failure.
pub(crate) fn failed(e: &impl fmt::Display) -> ToolFailure {
    ToolFailure::Failed(e.to_string())
}

/// A refusal as a tool failure.
pub(crate) fn refuse(reason: impl Into<String>) -> ToolFailure {
    ToolFailure::Refused(qm_types::error::Refusal::new(reason))
}
