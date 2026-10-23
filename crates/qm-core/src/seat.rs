//! The seat's engine session: opening, resuming and replacing the Quartermaster's session, and
//! memory across relaunches.
//!
//! [`Seat`] implements the seam's [`SeatSession`] for the turn loop:
//!
//! - **`open`** resumes the stored session when the app starts and the stored engine and account
//!   still serve the seat's model; otherwise it opens a new session, and a seat that has had turns
//!   before hands the loop a summary of the last few turns to put first in the next turn's input
//!   (written by code from the stored conversation).
//! - **`session_ready`** stores the engine's session reference on the Quartermaster's row, so the
//!   next launch can resume it, and checks that every Quartermaster tool reached the session.
//! - **`idle`** applies a change of the seat's model or effort between turns: `SetModel` within
//!   the same engine and account, the same conversation resumed at a new effort level, a new
//!   session for another engine or account.
//!
//! Where the model comes from is behind [`SeatModel`]: the app's routing in the running app, a
//! [`FixedSeatModel`] for the developer seat.

use crate::routing::{self, SeatModel as RoutedChoice};
use crate::turn_io::{IdleOutcome, OpenReason, OpenedSession, SeatError, SeatSession, event_kind};
use jiff::tz::TimeZone;
use qm_engine::{
    AccountRef, Builtins, Engine, InitReport, SessionCommand, SessionHandle, SessionLimits,
    SessionSpec, SystemPrompt,
};
use qm_policy::limits::engine_limits;
use qm_policy::markers::neutralise;
use qm_prompts::session_summary::{
    self, REASON_CLOSED, REASON_EFFORT_CHANGE, REASON_ENGINE_SWITCH, REASON_RESUME_FAILED,
    REASON_STARTUP, SUMMARY_TURNS, Said, SummaryLine, Who,
};
use qm_store::accounts;
use qm_store::db::Db;
use qm_store::events::{self, NewEvent};
use qm_store::messages::{self, MessageRow};
use qm_store::seat::{self as seat_row, SeatAgent};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::data_dir::DataDir;
use qm_types::domain::{Effort, EngineKind, MessageKind, MessageSource, Role};
use qm_types::ids::{AgentId, ModelId, SessionRef};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::watch;

/// What `open` says when nothing can run the seat's model.
pub const NO_MODEL: &str =
    "No connected account can run the Quartermaster's model; connect one in Settings ▸ Accounts.";

/// The prompt's name in `qm_prompts::index`.
pub const PROMPT: &str = "quartermaster";

/// How the Claude engine names our tools in its init report.
const MCP_PREFIX: &str = "mcp__quartermaster__";

/// Where the seat's model comes from.
pub trait SeatModel: Send + Sync + fmt::Debug {
    /// The engine, account, model and effort the seat should run on now; `None` when nothing can
    /// serve it.
    fn pick(&self) -> BoxFuture<'_, Result<Option<SeatPick>, SeatError>>;

    /// Resolves when the settings behind [`Self::pick`] may have changed, so an idle seat can
    /// follow a change at once. Never, by default.
    fn changed(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

/// The seat's model, with the account and engine that serve it (the engine follows the account).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatPick {
    pub engine: EngineKind,
    pub account: AccountRef,
    pub model: ModelId,
    pub effort: Effort,
}

/// A fixed pick: the developer seat.
#[derive(Clone, Debug)]
pub struct FixedSeatModel(pub SeatPick);

impl SeatModel for FixedSeatModel {
    fn pick(&self) -> BoxFuture<'_, Result<Option<SeatPick>, SeatError>> {
        let pick = self.0.clone();
        Box::pin(async move { Ok(Some(pick)) })
    }
}

/// The seat's model from the app's routing: your setting (Auto by default) applied to the current
/// pool by `routing::quartermaster_choice`, the account read back from the store, and the engine
/// that follows the account.
///
/// The settings come through a `watch` channel, so a change you make in Settings applies at the
/// seat's next idle moment without rebuilding anything.
#[derive(Debug)]
pub struct RoutedSeatModel {
    db: Db,
    clock: Arc<dyn Clock>,
    config: watch::Receiver<Config>,
    /// The receiver [`SeatModel::changed`] waits on: its own, so its seen version is kept between
    /// waits and no change is missed.
    watched: tokio::sync::Mutex<watch::Receiver<Config>>,
    /// Which logins work: accounts with a broken one are left out of the pool.
    health: Arc<qm_auth::LoginHealth>,
    /// `true` once the startup pass of model detection has finished (see [`Self::with_detection`]).
    detected: Option<watch::Receiver<bool>>,
}

impl RoutedSeatModel {
    /// Before the startup pass of model detection has read every account's models (in the
    /// background), the pool can look empty even though an account is connected: the
    /// seat's first `open` came before the Claude account's models were read and said "No
    /// connected account". With this, a pick that finds nothing waits for that pass (`detected`
    /// turning `true`, or its sender going away) and looks again. Only the seat's pick waits;
    /// startup and the UI never do.
    #[must_use]
    pub fn with_detection(mut self, detected: watch::Receiver<bool>) -> Self {
        self.detected = Some(detected);
        self
    }

    /// The pick from the current pool, without waiting.
    async fn pick_now(&self) -> Result<Option<SeatPick>, SeatError> {
        let config = self.config.borrow().clone();
        let at = self.clock.now();
        let health = Arc::clone(&self.health);
        Ok(self
            .db
            .call(move |conn| {
                let Some(choice): Option<RoutedChoice> =
                    routing::quartermaster_choice(conn, &config, &health, at)?
                else {
                    return Ok(None);
                };
                let Some(account) = accounts::load_account(conn, choice.choice.account)? else {
                    return Ok(None);
                };
                Ok(Some(SeatPick {
                    engine: account.engine,
                    account: routing::account_ref(&account),
                    model: choice.account_model,
                    effort: choice.choice.effort,
                }))
            })
            .await?)
    }

    pub fn new(
        db: Db,
        clock: Arc<dyn Clock>,
        config: watch::Receiver<Config>,
        health: Arc<qm_auth::LoginHealth>,
    ) -> Self {
        Self {
            db,
            clock,
            watched: tokio::sync::Mutex::new(config.clone()),
            config,
            health,
            detected: None,
        }
    }
}

impl SeatModel for RoutedSeatModel {
    fn pick(&self) -> BoxFuture<'_, Result<Option<SeatPick>, SeatError>> {
        Box::pin(async move {
            if let Some(pick) = self.pick_now().await? {
                return Ok(Some(pick));
            }
            let Some(mut detected) = self.detected.clone() else {
                return Ok(None);
            };
            if *detected.borrow_and_update() {
                return Ok(None);
            }
            // An error means the detection task is gone: it has done all it will.
            let _ = detected.wait_for(|done| *done).await;
            self.pick_now().await
        })
    }

    fn changed(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut watched = self.watched.lock().await;
            if watched.changed().await.is_err() {
                // The settings can't change any more.
                std::future::pending::<()>().await;
            }
        })
    }
}

/// What the seat needs.
#[derive(Debug)]
pub struct SeatDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub data_dir: DataDir,
    /// Every engine the binary injected, by kind; the seat never names a concrete one.
    pub engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    pub model: Arc<dyn SeatModel>,
    /// Passive context readings for the UI; its tap goes on every session this opens.
    pub monitor: Arc<crate::context_monitor::ContextMonitor>,
}

/// The Quartermaster's session.
#[derive(Debug)]
pub struct Seat {
    db: Db,
    clock: Arc<dyn Clock>,
    data_dir: DataDir,
    engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    model: Arc<dyn SeatModel>,
    tz: TimeZone,
    monitor: Arc<crate::context_monitor::ContextMonitor>,
}

impl Seat {
    pub fn new(deps: SeatDeps) -> Self {
        Self {
            db: deps.db,
            clock: deps.clock,
            data_dir: deps.data_dir,
            engines: deps.engines,
            model: deps.model,
            tz: TimeZone::system(),
            monitor: deps.monitor,
        }
    }

    /// The Quartermaster's row, created on first run.
    pub async fn agent(&self) -> Result<SeatAgent, SeatError> {
        let home = self.data_dir.seat().home;
        let at = self.clock.now();
        Ok(self
            .db
            .call(move |conn| seat_row::ensure_quartermaster(conn, &home, at))
            .await?)
    }

    async fn pick(&self) -> Result<SeatPick, SeatError> {
        self.model
            .pick()
            .await?
            .ok_or_else(|| SeatError::Unavailable(NO_MODEL.to_owned()))
    }

    /// The spec for the seat's session: our prompt in full, only the Quartermaster's tools, no
    /// built-in tools, no skills, the orchestrators' limits, never shut down while idle.
    fn spec(
        &self,
        agent: AgentId,
        pick: &SeatPick,
        effort: Effort,
        resume: Option<SessionRef>,
    ) -> Result<SessionSpec, SeatError> {
        let prompt = qm_prompts::index::get(PROMPT).ok_or_else(|| {
            SeatError::Unavailable("The Quartermaster's prompt is missing from this build.".into())
        })?;
        let paths = self.data_dir.seat();
        let mut spec = SessionSpec::new(
            agent,
            Role::Quartermaster,
            pick.account.clone(),
            pick.model.clone(),
            effort,
            paths.home,
            paths.session,
            SystemPrompt::Replace {
                text: prompt.to_owned(),
            },
        );
        spec.resume = resume;
        spec.tools = qm_tools::registry::descriptors(Role::Quartermaster, None);
        spec.builtins = Builtins::None;
        spec.skills_dir = None;
        spec.idle_shutdown = None;
        spec.limits = engine_limits(Role::Quartermaster).map(|l| SessionLimits {
            max_output_tokens: l.max_output_tokens,
            max_retries: l.max_retries,
        });
        Ok(spec)
    }

    /// The seat's folders exist before an engine is pointed at them.
    fn make_folders(&self) -> Result<(), SeatError> {
        let paths = self.data_dir.seat();
        for dir in [&paths.home, &paths.session] {
            std::fs::create_dir_all(dir).map_err(|e| {
                SeatError::Unavailable(format!(
                    "The Quartermaster's folder {} couldn't be created: {e}",
                    dir.display()
                ))
            })?;
        }
        Ok(())
    }

    /// The summary a new session opens with, when the seat has had turns before.
    async fn preamble(
        &self,
        agent: AgentId,
        why: &OpenReason,
    ) -> Result<Option<String>, SeatError> {
        let Some(reason) = summary_reason(why) else {
            return Ok(None);
        };
        let (had_turns, rows) = self
            .db
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

/// The summary's reason for each way a session can be new; `None` after `/clear`, which opens
/// with no summary at all.
fn summary_reason(why: &OpenReason) -> Option<&'static str> {
    Some(match why {
        OpenReason::Startup => REASON_STARTUP,
        OpenReason::ResumeFailed { .. } => REASON_RESUME_FAILED,
        OpenReason::Closed { .. } => REASON_CLOSED,
        OpenReason::EngineSwitch => REASON_ENGINE_SWITCH,
        OpenReason::EffortChange => REASON_EFFORT_CHANGE,
        OpenReason::Cleared => return None,
    })
}

/// `seat.session_opened`'s `why`, with its detail.
fn why_payload(why: &OpenReason) -> serde_json::Value {
    match why {
        OpenReason::Startup => serde_json::json!({"why": "startup"}),
        OpenReason::ResumeFailed { message } => {
            serde_json::json!({"why": "resume_failed", "message": message})
        }
        OpenReason::Closed { reason } => serde_json::json!({"why": "closed", "reason": reason}),
        OpenReason::EngineSwitch => serde_json::json!({"why": "engine_switch"}),
        OpenReason::EffortChange => serde_json::json!({"why": "effort_change"}),
        OpenReason::Cleared => serde_json::json!({"why": "cleared"}),
    }
}

/// Our tools the engine's init report doesn't list, bare or as `mcp__quartermaster__<name>`.
fn missing_tools(init: &InitReport) -> Vec<String> {
    qm_tools::registry::descriptors(Role::Quartermaster, None)
        .into_iter()
        .map(|d| d.name)
        .filter(|name| {
            !init
                .tools
                .iter()
                .any(|t| t == name || t.strip_prefix(MCP_PREFIX).is_some_and(|bare| bare == name))
        })
        .collect()
}

impl SeatSession for Seat {
    fn model_settings_changed(&self) -> BoxFuture<'_, ()> {
        self.model.changed()
    }

    fn open(&self, why: OpenReason) -> BoxFuture<'_, Result<OpenedSession, SeatError>> {
        Box::pin(async move {
            let agent = self.agent().await?;
            let pick = self.pick().await?;

            // Resume at startup or for a new effort level, and only where the stored engine and
            // account still serve the seat. The level is always the one your settings give now.
            let resume = match (&why, &agent.session_ref) {
                (OpenReason::Startup | OpenReason::EffortChange, Some(session))
                    if agent.engine == Some(pick.engine)
                        && agent.account == Some(pick.account.id) =>
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
                self.preamble(agent.id, &why).await?
            };
            self.make_folders()?;
            let spec = self.spec(agent.id, &pick, effort, resume)?;
            let engine = self.engines.get(&pick.engine).ok_or_else(|| {
                SeatError::Unavailable(format!(
                    "The {} engine isn't available in this build, so the Quartermaster can't run on {}.",
                    pick.engine, pick.model
                ))
            })?;
            let handle = engine
                .open(spec)?
                .observed(self.monitor.tap(qm_types::api::AgentRef::Quartermaster));

            let at = self.clock.now();
            let id = agent.id;
            let engine_kind = pick.engine;
            let account = pick.account.id;
            let model = pick.model.clone();
            let mut payload = why_payload(&why);
            payload["engine"] = serde_json::json!(engine_kind);
            payload["model"] = serde_json::json!(model);
            payload["effort"] = serde_json::json!(effort);
            payload["resumed"] = serde_json::json!(resumed);
            let summary = if resumed {
                format!("The Quartermaster's session was resumed on {model}")
            } else {
                format!("The Quartermaster's session was opened on {model}")
            };
            tracing::info!(%effort, "{summary}");
            self.db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    seat_row::update_session(&tx, id, engine_kind, account, &model, effort)?;
                    if !resumed {
                        // The stored session is left behind: never resume it later.
                        seat_row::set_session_ref(&tx, id, None, at)?;
                    }
                    events::insert(
                        &tx,
                        &NewEvent {
                            kind: event_kind::SESSION_OPENED,
                            agent: Some(id),
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
                agent: agent.id,
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
        Box::pin(async move {
            let missing = missing_tools(init);
            if !missing.is_empty() {
                tracing::warn!(
                    ?missing,
                    "the Quartermaster's session doesn't offer every Quartermaster tool"
                );
            }
            let session = session.clone();
            let at = self.clock.now();
            self.db
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    seat_row::set_session_ref(&tx, agent, Some(&session), at)?;
                    if !missing.is_empty() {
                        let summary = format!(
                            "The Quartermaster's session is missing tools: {}",
                            missing.join(", ")
                        );
                        events::insert(
                            &tx,
                            &NewEvent {
                                kind: event_kind::SESSION_OPENED,
                                agent: Some(agent),
                                task: None,
                                turn: None,
                                summary: &summary,
                                payload: serde_json::json!({
                                    "stage": "ready",
                                    "session": session,
                                    "resumed": resumed,
                                    "missing_tools": missing,
                                }),
                                at,
                            },
                        )?;
                    }
                    tx.commit()?;
                    Ok(())
                })
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
            let Some(pick) = self.model.pick().await? else {
                // The next open says why, if it comes to that.
                return Ok(IdleOutcome::Unchanged);
            };
            let row = self.db.call(|conn| seat_row::quartermaster(conn)).await?;
            let Some(row) = row.filter(|r| r.id == agent) else {
                return Ok(IdleOutcome::Unchanged);
            };
            if row.engine != Some(pick.engine) || row.account != Some(pick.account.id) {
                // `SetModel` carries no account, so another account is a new session too.
                return Ok(IdleOutcome::Reopen(OpenReason::EngineSwitch));
            }
            if row.effort != Some(pick.effort) {
                // An engine fixes the level when its session starts: resume the conversation at
                // the new one (with the new model too, if that changed).
                return Ok(IdleOutcome::Reopen(OpenReason::EffortChange));
            }
            if row.model.as_ref() == Some(&pick.model) {
                return Ok(IdleOutcome::Unchanged);
            }
            handle.send(SessionCommand::SetModel {
                model: pick.model.clone(),
            })?;
            let model = pick.model;
            self.db
                .call(move |conn| seat_row::set_model(conn, agent, &model))
                .await?;
            Ok(IdleOutcome::Unchanged)
        })
    }
}
