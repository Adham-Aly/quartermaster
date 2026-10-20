//! Your messages and `Esc` to a secondmate, and the quit.
//!
//! `Send { to: Secondmate }` is direct intervention's secondmate arm
//! ([`reconcile::intervene_secondmate`]): your text (neutralised) goes into the secondmate's own
//! conversation as a turn you started there, and the Quartermaster hears of it; `Interrupt {
//! Secondmate }` stops its running turn only. The quit asks first while a secondmate has a turn
//! running, then shuts every loop down.

use super::SecondmateService;
use crate::api::{ApiPart, Handled};
use crate::attachments::Attachments;
use crate::reconcile::{self, Intervention};
use qm_types::BoxFuture;
use qm_types::api::{AgentRef, Command, Snapshot};

/// The secondmates' API part.
#[derive(Clone, Debug)]
pub struct SecondmatesPart {
    svc: SecondmateService,
    /// Where the images pasted into your messages to a secondmate are saved.
    attachments: Attachments,
}

impl SecondmatesPart {
    pub fn new(svc: SecondmateService, attachments: Attachments) -> Self {
        Self { svc, attachments }
    }
}

impl ApiPart for SecondmatesPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        match command {
            Command::Send {
                to: AgentRef::Secondmate { id },
                text,
                images,
            } => {
                let id = *id;
                Some(Box::pin(async move {
                    let staged = self.attachments.stage(text, images).await;
                    match reconcile::intervene_secondmate(&self.svc, id, text.clone(), staged).await
                    {
                        Ok(()) => Handled::Changed,
                        Err(Intervention::Blank | Intervention::Closed { .. }) => {
                            Handled::Unchanged
                        }
                        Err(Intervention::NotAvailable) => {
                            tracing::info!(%id, "the secondmate isn't running; your message wasn't sent");
                            Handled::Unchanged
                        }
                    }
                }))
            }
            Command::Interrupt {
                agent: AgentRef::Secondmate { id },
            } => {
                let id = *id;
                Some(Box::pin(async move {
                    self.svc.interrupt(id);
                    Handled::Unchanged
                }))
            }
            // Asked only; the loops stop on `Quit`, or when serving ends, so a quit you cancel
            // because of another part's question leaves them running.
            Command::RequestQuit => Some(Box::pin(async move {
                let working = self.svc.working_count();
                if working > 0 {
                    Handled::ConfirmQuit { working }
                } else {
                    Handled::Unchanged
                }
            })),
            Command::Quit => Some(Box::pin(async move {
                self.svc.shutdown().await;
                Handled::Unchanged
            })),
            _ => None,
        }
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        // The crew view draws the secondmate rows; their conversations are the session view's.
        let _ = snapshot;
        Box::pin(async {})
    }
}
