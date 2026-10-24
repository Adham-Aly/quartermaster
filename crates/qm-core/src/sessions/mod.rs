//! Worker sessions.
//!
//! - [`recorder`]: the task service's session sink, writing `session_lines`.
//! - [`view`]: the session as its screen shows it (`Snapshot.session`).
//! - [`SessionsPart`]: the API's side: `Command::ViewSession` remembers which session is open
//!   (and the snapshot then carries it); `Command::Send` to a crewmate is direct intervention
//!   ([`crate::reconcile::intervene`]); to a secondmate, `SecondmatesPart` handles it.

pub mod recorder;
pub mod view;

use crate::api::{ApiPart, Handled};
use crate::attachments::Attachments;
use crate::reconcile::{self, Intervention, ReconcileService};
use crate::secondmates::SecondmateService;
use qm_store::db::Db;
use qm_types::BoxFuture;
use qm_types::api::{AgentRef, Command, Snapshot};
use qm_types::clock::Clock;
use recorder::Recorder;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

/// The worker-session commands and `Snapshot.session`.
pub struct SessionsPart {
    db: Db,
    clock: Arc<dyn Clock>,
    reconcile: ReconcileService,
    recorder: Recorder,
    /// Where the images pasted into your messages to a crewmate are saved.
    attachments: Attachments,
    /// For a secondmate's session.
    secondmates: Option<SecondmateService>,
    /// The session you have open, if any.
    open: Mutex<Option<AgentRef>>,
}

impl fmt::Debug for SessionsPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionsPart")
            .field("open", &self.open())
            .finish_non_exhaustive()
    }
}

impl SessionsPart {
    pub fn new(
        db: Db,
        clock: Arc<dyn Clock>,
        reconcile: ReconcileService,
        recorder: Recorder,
        attachments: Attachments,
    ) -> Self {
        Self {
            db,
            clock,
            reconcile,
            recorder,
            attachments,
            secondmates: None,
            open: Mutex::new(None),
        }
    }

    /// Secondmates' sessions open too.
    #[must_use]
    pub fn with_secondmates(mut self, secondmates: SecondmateService) -> Self {
        self.secondmates = Some(secondmates);
        self
    }

    fn open(&self) -> Option<AgentRef> {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_open(&self, agent: Option<AgentRef>) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = agent;
    }
}

impl ApiPart for SessionsPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::ViewSession { agent } => {
                // The Quartermaster's conversation is the Bridge.
                let agent = agent.filter(|a| *a != AgentRef::Quartermaster);
                Some(Box::pin(async move {
                    self.set_open(agent);
                    Handled::Changed
                }))
            }
            Command::Send {
                to: to @ AgentRef::Crewmate { .. },
                text,
                images,
            } => Some(Box::pin(async move {
                let staged = self.attachments.stage(text, images).await;
                match reconcile::intervene(&self.reconcile, *to, text.clone(), staged).await {
                    Ok(()) | Err(Intervention::Closed { .. }) => Handled::Changed,
                    Err(Intervention::Blank) => Handled::Unchanged,
                    Err(Intervention::NotAvailable) => {
                        tracing::info!(?to, "messages to this agent aren't available yet");
                        Handled::Unchanged
                    }
                }
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(agent) = self.open() else {
                return;
            };
            if let AgentRef::Secondmate { id } = agent {
                let Some(secondmates) = &self.secondmates else {
                    return;
                };
                match view::load_secondmate(&self.db, secondmates, id).await {
                    Ok(view) => snapshot.session = view,
                    Err(e) => {
                        tracing::error!(%id, error = %e, "couldn't read the secondmate's session");
                    }
                }
                return;
            }
            match view::load(&self.db, &self.recorder, agent, &*self.clock).await {
                Ok(view) => snapshot.session = view,
                Err(e) => tracing::error!(error = %e, "couldn't read the worker's session"),
            }
        })
    }
}
