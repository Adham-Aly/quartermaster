//! A secondmate's engine session.
//!
//! [`SecondmateSeat`] implements the turn loop's [`SeatSession`] as `seat::Seat` does for the
//! Quartermaster, with these differences: the role is `Secondmate`; the prompt is
//! `qm_prompts::charter::secondmate_prompt` with the charter loaded **at each open** (so a reopened
//! session carries the current charter); the tools are the secondmate's; the session closes after
//! [`SECONDMATE_IDLE_SHUTDOWN`] of quiet and resumes on the next turn; and what the session runs on
//! is stored on the secondmate's own agent row. The model is the Quartermaster's (`SeatModel`):
//! secondmates follow the same setting.

use super::SecondmateService;
use super::charter::charter_text;
use crate::seat::SeatPick;
use crate::turn_io::{IdleOutcome, OpenReason, OpenedSession, SeatError, SeatSession, event_kind};
use jiff::tz::TimeZone;
use qm_engine::{
    Builtins, InitReport, SessionCommand, SessionHandle, SessionLimits, SessionSpec, SystemPrompt,
};
use qm_policy::limits::engine_limits;
use qm_policy::markers::neutralise;
use qm_prompts::charter::{CharterFacts, secondmate_prompt};
use qm_prompts::session_summary::{
    self, REASON_CLOSED, REASON_EFFORT_CHANGE, REASON_ENGINE_SWITCH, REASON_RESUME_FAILED,
    REASON_STARTUP, SUMMARY_TURNS, Said, SummaryLine, Who,
};
use qm_store::events::{self, NewEvent};
use qm_store::messages::{self, MessageRow};
use qm_store::secondmates::{self, SecondmateRecord};
use qm_types::BoxFuture;
use qm_types::domain::{Effort, MessageKind, MessageSource, Role};
use qm_types::ids::{AgentId, SecondmateId, SessionRef};
use std::fmt;
use std::time::Duration;

/// An idle secondmate holds no process: its session closes after this long without a turn and
/// resumes on the next.
pub const SECONDMATE_IDLE_SHUTDOWN: Duration = Duration::from_mins(5);

/// What `open` says when nothing can run the model.
pub const NO_MODEL: &str =
    "No connected account can run the secondmate's model; connect one in Settings ▸ Accounts.";

/// One secondmate's session.
pub struct SecondmateSeat {
    svc: SecondmateService,
    id: SecondmateId,
    tz: TimeZone,
}

impl fmt::Debug for SecondmateSeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondmateSeat")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SecondmateSeat {
    pub fn new(svc: SecondmateService, id: SecondmateId) -> Self {
        Self {
            svc,
            id,
            tz: TimeZone::system(),
        }
    }

    async fn record(&self) -> Result<SecondmateRecord, SeatError> {
        let id = self.id;
        self.svc
            .db()
            .call(move |conn| secondmates::load(conn, id))
            .await?
            .ok_or_else(|| SeatError::Unavailable(format!("There's no secondmate {id}.")))
    }

    async fn pick(&self) -> Result<SeatPick, SeatError> {
        self.svc
            .deps()
            .model
            .pick()
            .await?
            .ok_or_else(|| SeatError::Unavailable(NO_MODEL.to_owned()))
    }

    fn spec(
        &self,
        record: &SecondmateRecord,
        pick: &SeatPick,
        effort: Effort,
        resume: Option<SessionRef>,
    ) -> SessionSpec {
        let names: Vec<&str> = record.projects.iter().map(|(_, n)| n.as_str()).collect();
        let prompt = secondmate_prompt(&CharterFacts {
            id: record.id,
            name: &record.name,
            scope: &record.scope,
            projects: &names,
            standing_branch: record.standing_branch.as_deref(),
        });
        let paths = self.svc.deps().data_dir.secondmate(self.id);
        let mut spec = SessionSpec::new(
            record.agent,
            Role::Secondmate,
            pick.account.clone(),
            pick.model.clone(),
            effort,
            paths.home,
            paths.session,
            SystemPrompt::Replace { text: prompt },
        );
        spec.resume = resume;
        spec.transcript_dir = Some(paths.transcript);
        spec.protected_roots = self.svc.deps().data_dir.protected_roots();
        spec.tools = qm_tools::registry::descriptors(Role::Secondmate, None);
        spec.builtins = Builtins::None;
        spec.skills_dir = None;
        spec.idle_shutdown = Some(SECONDMATE_IDLE_SHUTDOWN);
        spec.limits = engine_limits(Role::Secondmate).map(|l| SessionLimits {
            max_output_tokens: l.max_output_tokens,
            max_retries: l.max_retries,
        });
        spec
    }

    fn make_folders(&self) -> Result<(), SeatError> {
        use std::os::unix::fs::DirBuilderExt;
        let paths = self.svc.deps().data_dir.secondmate(self.id);
        for dir in [&paths.home, &paths.session, &paths.transcript] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| {
                    SeatError::Unavailable(format!(
                        "{}'s folder {} couldn't be created: {e}",
                        self.id,
                        dir.display()
                    ))
                })?;
        }
        Ok(())
    }

    /// The summary a new session opens with, from its own conversation.
    async fn preamble(
        &self,
        agent: AgentId,
        why: &OpenReason,
    ) -> Result<Option<String>, SeatError> {
        if *why == OpenReason::Cleared {
            return Ok(None);
        }
        let (had_turns, rows) = self
            .svc
            .db()
            .call(move |conn| {
                if !messages::has_turns(conn, agent)? {
                    return Ok((false, Vec::new()));
                }
                Ok((true, messages::for_summary(conn, agent, SUMMARY_TURNS)?))
            })
            .await?;
        if !had_turns {
            return Ok(None);
        }
        let lines: Vec<SummaryLine> = rows.iter().filter_map(|m| self.summary_line(m)).collect();
        let reason = match why {
            OpenReason::Startup => REASON_STARTUP,
            OpenReason::ResumeFailed { .. } => REASON_RESUME_FAILED,
            OpenReason::Closed { .. } => REASON_CLOSED,
            OpenReason::EngineSwitch => REASON_ENGINE_SWITCH,
            OpenReason::EffortChange => REASON_EFFORT_CHANGE,
            OpenReason::Cleared => return Ok(None),
        };
        Ok(Some(session_summary::render(reason, &lines)))
    }

    fn summary_line(&self, m: &MessageRow) -> Option<SummaryLine> {
        let who = match (m.source, m.kind) {
            (MessageSource::You, MessageKind::Message) => Who::You,
            (MessageSource::Agent, MessageKind::Reply) => Who::Quartermaster(Said::Replied),
            (MessageSource::Agent, MessageKind::Question) => Who::Quartermaster(Said::Asked),
            (MessageSource::Agent, MessageKind::Acknowledgement) => {
                Who::Quartermaster(Said::Acknowledged)
            }
            (MessageSource::App, MessageKind::Acknowledgement) => Who::App,
            _ => return None,
        };
        let mut text = m.text.clone();
        if !m.options.is_empty() {
            text = format!("{text} (options: {})", m.options.join(" · "));
        }
        Some(SummaryLine {
            at: m.at.to_zoned(self.tz.clone()),
            who,
            text: neutralise(&text),
        })
    }
}

impl SeatSession for SecondmateSeat {
    fn open(&self, why: OpenReason) -> BoxFuture<'_, Result<OpenedSession, SeatError>> {
        Box::pin(async move {
            let record = self.record().await?;
            let id = self.id;
            let row = self
                .svc
                .db()
                .call(move |conn| secondmates::session_row(conn, id))
                .await?
                .ok_or_else(|| SeatError::Unavailable(format!("{id} has no agent row.")))?;
            let pick = self.pick().await?;

            // Resume where the stored engine and account still serve the model; an idle
            // shutdown is resumed by the engine itself on the next turn.
            // A new effort level resumes the conversation too; the level is always the one the
            // Quartermaster's settings give now.
            let resume = match (&why, &row.session_ref) {
                (OpenReason::Startup | OpenReason::EffortChange, Some(session))
                    if row.engine == Some(pick.engine) && row.account == Some(pick.account.id) =>
                {
                    Some(session.clone())
                }
                _ => None,
            };
            let resumed = resume.is_some();
            let effort = pick.effort;
            let preamble = if resumed {
                None
            } else {
                self.preamble(row.id, &why).await?
            };
            self.make_folders()?;
            let spec = self.spec(&record, &pick, effort, resume);
            let engine = self.svc.deps().engines.get(&pick.engine).ok_or_else(|| {
                SeatError::Unavailable(format!(
                    "The {} engine isn't available in this build, so {id} can't run on {}.",
                    pick.engine, pick.model
                ))
            })?;
            let handle = engine.open(spec)?.observed(
                self.svc
                    .deps()
                    .monitor
                    .tap(qm_types::api::AgentRef::Secondmate { id }),
            );

            let at = self.svc.clock().now();
            let agent = row.id;
            let engine_kind = pick.engine;
            let account = pick.account.id;
            let model = pick.model.clone();
            let summary = if resumed {
                format!("{id}'s session was resumed on {model}")
            } else {
                format!("{id}'s session was opened on {model}")
            };
            let payload = serde_json::json!({
                "secondmate": id,
                "engine": engine_kind,
                "model": model,
                "effort": effort,
                "resumed": resumed,
                "charter": charter_text(&record),
            });
            self.svc
                .db()
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    secondmates::update_session(&tx, agent, engine_kind, account, &model, effort)?;
                    if !resumed {
                        secondmates::set_session_ref(&tx, agent, None, at)?;
                    }
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: event_kind::SESSION_OPENED,
                            agent: Some(agent),
                            task: None,
                            turn: None,
                            summary: &summary,
                            payload,
                            at,
                        },
                    )?;
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            Ok(OpenedSession {
                agent,
                handle,
                preamble,
            })
        })
    }

    fn session_ready<'a>(
        &'a self,
        agent: AgentId,
        session: &'a SessionRef,
        resumed: bool,
        init: &'a InitReport,
    ) -> BoxFuture<'a, Result<(), SeatError>> {
        let _ = (resumed, init);
        Box::pin(async move {
            let session = session.clone();
            let at = self.svc.clock().now();
            self.svc
                .db()
                .call(move |conn| secondmates::set_session_ref(conn, agent, Some(&session), at))
                .await?;
            Ok(())
        })
    }

    fn idle<'a>(
        &'a self,
        agent: AgentId,
        handle: &'a SessionHandle,
    ) -> BoxFuture<'a, Result<IdleOutcome, SeatError>> {
        Box::pin(async move {
            let Some(pick) = self.svc.deps().model.pick().await? else {
                return Ok(IdleOutcome::Unchanged);
            };
            let id = self.id;
            let row = self
                .svc
                .db()
                .call(move |conn| secondmates::session_row(conn, id))
                .await?;
            let Some(row) = row.filter(|r| r.id == agent) else {
                return Ok(IdleOutcome::Unchanged);
            };
            if row.engine != Some(pick.engine) || row.account != Some(pick.account.id) {
                return Ok(IdleOutcome::Reopen(OpenReason::EngineSwitch));
            }
            if row.effort != Some(pick.effort) {
                return Ok(IdleOutcome::Reopen(OpenReason::EffortChange));
            }
            if row.model.as_ref() == Some(&pick.model) {
                return Ok(IdleOutcome::Unchanged);
            }
            handle.send(SessionCommand::SetModel {
                model: pick.model.clone(),
            })?;
            let model = pick.model;
            self.svc
                .db()
                .call(move |conn| secondmates::set_model(conn, agent, &model))
                .await?;
            Ok(IdleOutcome::Unchanged)
        })
    }
}
