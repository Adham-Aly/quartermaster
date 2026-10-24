//! Proposing a secondmate and changing its charter.
//!
//! - **Proposing** ([`SecondmateService::propose`]) is always a held action: the exact payload is
//!   stored for your approval and nothing is created before that. The [`SecondmatePerformer`]
//!   performs exactly the stored payload on your yes, never the tool's arguments.
//! - **Changing the charter** ([`SecondmateService::configure`]): scope words and projects on any
//!   Quartermaster turn; the standing branch only on a turn you started (the handler checks it). A
//!   change affects only crewmates started afterwards: running tasks keep their recorded base, and
//!   routed requests their own branch. The secondmate learns of it from a `charter_changed` wake
//!   carrying the new charter; its next session's prompt carries it too.

use super::{SecondmateService, event_kind, failed, refuse};
use crate::held::{HeldCard, HeldError, HeldOutcome, HeldPerformer, HeldService, LoadedHeld};
use crate::inbox::{Inbox, Notify};
use qm_policy::scope::{ReservedAction, check_own_secondmate, check_reserved};
use qm_prompts::charter::{CharterFacts, render_charter};
use qm_routing::decider::{Decided, SecondmateQuestion};
use qm_store::StoreError;
use qm_store::events::{self, NewEvent};
use qm_store::held::{HeldPayload, NewHeld, SecondmatePayload};
use qm_store::messages;
use qm_store::secondmates::{self, NewSecondmate, SecondmateRecord};
use qm_store::wakes::NewWake;
use qm_tools::defs::{Ack, Held};
use qm_tools::handler::{Caller, CharterChange, SecondmateProposal, ToolFailure};
use qm_types::BoxFuture;
use qm_types::api::{CardLine, HeldAnswer, Styled, Tone};
use qm_types::domain::WakeKind;
use qm_types::error::Refusal;
use rusqlite::Connection;
use std::fmt::Write as _;
use std::path::Path;

/// The charter text for a record.
pub fn charter_text(record: &SecondmateRecord) -> String {
    let names: Vec<&str> = record.projects.iter().map(|(_, n)| n.as_str()).collect();
    render_charter(&CharterFacts {
        id: record.id,
        name: &record.name,
        scope: &record.scope,
        projects: &names,
        standing_branch: record.standing_branch.as_deref(),
    })
}

/// "There's already a secondmate called web-app (S-02)".
fn name_refusal(conn: &Connection, name: &str) -> Result<Option<Refusal>, StoreError> {
    Ok(secondmates::by_name(conn, name)?
        .map(|id| Refusal::new(format!("There's already a secondmate called {name} ({id})"))))
}

impl SecondmateService {
    /// `start_secondmate`: checks the facts, asks the decider, and stores the exact proposal as a
    /// held action, listed in Actions. Nothing else is created.
    #[allow(clippy::too_many_lines)]
    pub async fn propose(
        &self,
        caller: &Caller,
        proposal: SecondmateProposal,
    ) -> Result<Held, ToolFailure> {
        check_reserved(caller.role, ReservedAction::StartSecondmate)?;
        let message = caller.turn.and_then(|t| t.started_by.message());
        let name = proposal.name.clone();
        let user_words = self
            .db()
            .call(move |conn| {
                if let Some(r) = name_refusal(conn, &name)? {
                    return Ok(Err(r));
                }
                let words = match message {
                    Some(m) => messages::text(conn, m)?.unwrap_or_default(),
                    None => String::new(),
                };
                Ok(Ok(words))
            })
            .await
            .map_err(|e| failed(&e))??;

        // The Quartermaster calling the tool is its declaration.
        let decided = self
            .deps()
            .decider
            .warrants_secondmate(SecondmateQuestion {
                declared: true,
                request: &proposal.scope,
                user_words: &user_words,
            })
            .await;
        let decided_by = match &decided {
            Decided::Answer { value: false, .. } => {
                return Err(refuse(
                    "the decision model doesn't think this needs a secondmate; start the work \
                     with start_crewmate",
                ));
            }
            Decided::Answer { by, .. } => by.to_string(),
            Decided::Undecided { reason } => format!("undecided ({reason})"),
        };

        // Stored with the registered names, exactly as they will be performed.
        let payload = SecondmatePayload {
            name: proposal.name.clone(),
            scope: proposal.scope.clone(),
            base_branch: proposal.base_branch.clone(),
        };
        let mut summary = format!(
            "Proposed secondmate {} · scope \"{}\"",
            payload.name, payload.scope
        );
        if let Some(b) = &payload.base_branch {
            let _ = write!(summary, " · base {b}");
        }
        let agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.clock().now();
        let title = format!("New secondmate: {}", payload.name);
        let event_payload = serde_json::json!({
            "name": payload.name,
            "scope": payload.scope,
            "base_branch": payload.base_branch,
            "decided_by": decided_by,
        });
        let outcome = self
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                // The name may have been taken while the decider answered.
                if let Some(r) = name_refusal(&tx, &payload.name)? {
                    return Ok(Err(r));
                }
                let held = HeldService::insert(
                    &tx,
                    &NewHeld {
                        payload: HeldPayload::Secondmate(payload),
                        choices: Vec::new(),
                        title,
                        task: None,
                        asked_by: Some(agent),
                        at,
                    },
                )?;
                events::insert(
                    &tx,
                    &NewEvent {
                        kind: event_kind::PROPOSED,
                        agent: Some(agent),
                        task: None,
                        turn,
                        summary: &summary,
                        payload: {
                            let mut p = event_payload;
                            p["held"] = serde_json::json!(held);
                            p
                        },
                        at,
                    },
                )?;
                tx.commit()?;
                Ok(Ok(held))
            })
            .await
            .map_err(|e| failed(&e))??;
        Ok(Held {
            held_action: outcome,
            note: "Held for the user's confirmation. Nothing is created until they approve; \
                   you'll be woken with the outcome."
                .to_owned(),
        })
    }

    /// `configure_secondmate`: the caller's own secondmate only; one transaction for the change,
    /// its event and the `charter_changed` wake to the secondmate.
    pub async fn configure(
        &self,
        caller: &Caller,
        change: CharterChange,
    ) -> Result<Ack, ToolFailure> {
        let caller_agent = caller.agent;
        let turn = caller.turn.map(|t| t.turn);
        let at = self.clock().now();
        let outcome = self
            .db()
            .call(move |conn| configure_in(conn, caller_agent, turn, &change, at))
            .await
            .map_err(|e| failed(&e))?;
        let (notify, note) = outcome?;
        self.deps().inbox.notify(&notify);
        Ok(Ack { note })
    }
}

/// [`SecondmateService::configure`]'s transaction.
fn configure_in(
    conn: &mut Connection,
    caller: qm_types::ids::AgentId,
    turn: Option<qm_types::ids::TurnId>,
    change: &CharterChange,
    at: jiff::Timestamp,
) -> Result<Result<(Notify, String), ToolFailure>, StoreError> {
    let id = change.secondmate;
    let tx = conn.transaction()?;
    let Some(before) = secondmates::load(&tx, id)? else {
        return Ok(Err(refuse(format!("There's no secondmate {id}"))));
    };
    if let Err(r) = check_own_secondmate(caller, id, before.parent) {
        return Ok(Err(r.into()));
    }
    let mut said: Vec<String> = Vec::new();
    if let Some(scope) = &change.scope {
        secondmates::set_scope(&tx, id, scope)?;
        said.push(format!("scope \"{}\" → \"{scope}\"", before.scope));
    }
    if let Some(branch) = &change.standing_branch {
        secondmates::set_standing_branch(&tx, id, branch.as_deref())?;
        let from = before.standing_branch.as_deref().unwrap_or("none");
        let to = branch.as_deref().unwrap_or("none");
        said.push(format!("base {from} → {to}"));
    }
    let Some(after) = secondmates::load(&tx, id)? else {
        return Ok(Err(refuse(format!("There's no secondmate {id}"))));
    };
    let summary = format!("{id} charter: {}", said.join(" · "));
    let event = events::insert(
        &tx,
        &NewEvent {
            kind: event_kind::CHARTER_CHANGED,
            agent: Some(caller),
            task: None,
            turn,
            summary: &summary,
            payload: serde_json::json!({
                "secondmate": id,
                "scope": after.scope,
                "projects": after.projects.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
                "standing_branch": after.standing_branch,
            }),
            at,
        },
    )?;
    let mut notify = Notify::new();
    Inbox::post(
        &tx,
        &NewWake {
            target: after.agent,
            kind: WakeKind::CharterChanged,
            payload: serde_json::json!({
                "text": format!("Your charter changed. {}", charter_text(&after)),
                "short": format!("{id} charter changed"),
            }),
            dedupe_key: format!("charter:{id}:{event}"),
            task: None,
            at,
        },
        &mut notify,
    )?;
    tx.commit()?;
    Ok(Ok((
        notify,
        format!(
            "{summary}. It applies to crewmates {id} starts from now on; running tasks keep their \
             base."
        ),
    )))
}

/// Carries out an approved secondmate proposal exactly as stored.
#[derive(Debug)]
pub struct SecondmatePerformer {
    svc: SecondmateService,
}

impl SecondmatePerformer {
    pub fn new(svc: SecondmateService) -> Self {
        Self { svc }
    }
}

/// Makes a secondmate's folders, owner-only.
fn make_folders(paths: &[&Path]) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    for dir in paths {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    Ok(())
}

impl HeldPerformer for SecondmatePerformer {
    fn perform<'a>(
        &'a self,
        held: &'a LoadedHeld,
        answer: &'a HeldAnswer,
    ) -> BoxFuture<'a, Result<HeldOutcome, HeldError>> {
        Box::pin(async move {
            let HeldPayload::Secondmate(payload) = &held.payload else {
                return Err(HeldError::Failed(
                    "this card isn't a secondmate proposal".to_owned(),
                ));
            };
            if *answer != HeldAnswer::Approve {
                return Err(Refusal::new("a secondmate proposal takes Approve or Reject").into());
            }
            let Some(parent) = held.asked_by else {
                return Err(HeldError::Failed(
                    "this proposal doesn't say who asked for it".to_owned(),
                ));
            };
            let payload = payload.clone();
            let data_dir = self.svc.deps().data_dir.clone();
            let at = self.svc.clock().now();
            let created = self
                .svc
                .db()
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    // What may have moved since the proposal.
                    if let Some(id) = secondmates::by_name(&tx, &payload.name)? {
                        return Ok(Err(format!(
                            "There's already a secondmate called {} ({id})",
                            payload.name
                        )));
                    }
                    // The standing branch exactly as stored: authorised by your turn when
                    // proposed, and approving the card is your act too.
                    let record = secondmates::insert(
                        &tx,
                        &NewSecondmate {
                            name: &payload.name,
                            scope: &payload.scope,
                            projects: &[],
                            standing_branch: payload.base_branch.as_deref(),
                            parent,
                            data_dir: &data_dir,
                            at,
                        },
                    )?;
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: event_kind::CREATED,
                            agent: Some(record.agent),
                            task: None,
                            turn: None,
                            summary: &format!(
                                "{} {} created · scope \"{}\"",
                                record.id, record.name, record.scope
                            ),
                            payload: serde_json::json!({
                                "secondmate": record.id,
                                "name": record.name,
                                "scope": record.scope,
                                "standing_branch": record.standing_branch,
                            }),
                            at,
                        },
                    )?;
                    tx.commit()?;
                    Ok(Ok(record))
                })
                .await?
                .map_err(HeldError::Failed)?;
            let paths = self.svc.deps().data_dir.secondmate(created.id);
            if let Err(e) = make_folders(&[&paths.root, &paths.home, &paths.session]) {
                tracing::warn!(id = %created.id, error = %e, "couldn't make the secondmate's folders");
            }
            self.svc.spawn(created.id).await;
            Ok(HeldOutcome::new(format!(
                "Created {} ({}). Route requests in its scope to it with route_to_secondmate.",
                created.id, created.name
            )))
        })
    }

    fn card(&self, held: &LoadedHeld) -> HeldCard {
        let HeldPayload::Secondmate(p) = &held.payload else {
            return HeldCard::default();
        };
        let dim = |s: &str| Styled::new(s, Tone::Dim);
        let text = |s: String| Styled::new(s, Tone::Text);
        let line = |label: &str, value: String| CardLine::new(2, vec![dim(label), text(value)]);
        HeldCard {
            note: Some("· secondmate".to_owned()),
            lines: vec![
                line("Name ", p.name.clone()),
                line("Scope ", p.scope.clone()),
                line(
                    "Standing base branch ",
                    p.base_branch.clone().unwrap_or_else(|| "none".to_owned()),
                ),
            ],
            buttons: Vec::new(),
        }
    }
}
