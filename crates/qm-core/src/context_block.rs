//! Gathering the context block.
//!
//! [`ContextBuilder`] implements the seam's [`ContextSource`]: one database call reads every
//! section from the store (`qm_store::context`, `events`, `standing_instructions`), the rows become
//! `qm_prompts::context_block::ContextFacts`, and the prompt crate renders them. The block is
//! rebuilt in full every turn, so it is always current whatever the session remembers.
//!
//! **"Only what changed."** The since-sections list events after the previous turn's
//! `seat.turn_started` (the watermark) and before this turn's, so everything that happened during
//! the previous turn shows exactly once. Which section an event lands in is its kind's prefix
//! ([`YOU`], [`ACTIVITY`], [`MODEL_CHANGES`], [`OUTCOMES`]); its `summary` is the line shown.
//! `seat.*` events never appear.
//!
//! Text that came from outside the app (titles, scopes, your instructions, event lines) is
//! neutralised here before rendering, so nothing in the block can open or close a marker.

use crate::turn_io::{ContextSource, SeatError};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qm_policy::markers::neutralise;
use qm_prompts::context_block::{
    self as block, ContextFacts, CrewmateFact, EventLine, PREMIUM_NOTE, ProjectFact,
    RECENT_OUTCOMES, SINCE_LINES, SecondmateFact, Since, TaskFact, TierModels, UserMessageFact,
    WorkerFact,
};
use qm_store::StoreError;
use qm_store::context::{self, CrewmateRow, ProjectRow, TaskRow, TierRow, Watermark, WorkerRow};
use qm_store::db::Db;
use qm_store::events::{self, EventPage, EventRow};
use qm_store::standing_instructions;
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::domain::{KindRow, TaskKind, TaskStatus, Tier};
use qm_types::ids::{AgentId, StandingInstructionId, TurnId};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

/// `<you>`: your own actions and choices.
pub const YOU: &[&str] = &["you."];
/// `<activity>`: new replies and activity on tasks.
pub const ACTIVITY: &[&str] = &["task.", "pr.", "checks.", "secondmate."];
/// `<model_changes>`: changes to the models available. `model_new` and `model_gone` are the kinds
/// `qm_store::models` announces with today; `model.` is the convention.
pub const MODEL_CHANGES: &[&str] = &["model.", "model_new", "model_gone"];
/// `<recent_outcomes>`.
pub const OUTCOMES: &str = "outcome.";

/// Builds the Quartermaster's context block each turn.
#[derive(Debug)]
pub struct ContextBuilder {
    db: Db,
    clock: Arc<dyn Clock>,
    tz: TimeZone,
    home: Option<PathBuf>,
    config: Option<watch::Receiver<Config>>,
}

impl ContextBuilder {
    /// Times in the system's time zone; paths under your home folder shown with `~`.
    pub fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self {
            db,
            clock,
            config: None,
            tz: TimeZone::system(),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_absolute()),
        }
    }

    /// Read live Task kinds settings each turn; without a source, use the app defaults.
    #[must_use]
    pub fn with_config(mut self, config: watch::Receiver<Config>) -> Self {
        self.config = Some(config);
        self
    }

    /// The same builder with another home folder for `~` (or none).
    #[must_use]
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// The facts for `turn` of `agent`, as the block will show them.
    pub async fn facts(&self, agent: AgentId, turn: TurnId) -> Result<ContextFacts, SeatError> {
        let gathered = self.db.call(move |conn| gather(conn, agent, turn)).await?;
        Ok(self.to_facts(turn, self.clock.now(), &gathered))
    }

    fn to_facts(&self, turn: TurnId, now: Timestamp, g: &Gathered) -> ContextFacts {
        let line = |e: &EventRow| EventLine {
            at: e.at.to_zoned(self.tz.clone()),
            text: neutralise(&e.summary),
        };
        let since = |page: &EventPage| Since {
            lines: page.rows.iter().map(line).collect(),
            more: page.more,
        };
        let workers: Vec<WorkerFact> = g.workers.iter().map(|w| worker_fact(w, now)).collect();
        let tasks: Vec<TaskFact> = g.tasks.iter().map(task_fact).collect();
        ContextFacts {
            turn,
            at: now.to_zoned(self.tz.clone()),
            projects: g.projects.iter().map(|p| self.project_fact(p)).collect(),
            workers_total: workers.len(),
            workers,
            tasks_open: tasks.len(),
            tasks,
            since_turn: g.watermark.map(|w| w.turn),
            you: since(&g.you),
            activity: since(&g.activity),
            model_changes: since(&g.model_changes),
            recent_outcomes: g.outcomes.iter().map(line).collect(),
            standing: g
                .standing
                .iter()
                .map(|(id, text)| (*id, neutralise(text)))
                .collect(),
            models: g.tiers.iter().map(tier_models).collect(),
            user_message_sources: g
                .user_message_sources
                .iter()
                .map(|s| UserMessageFact {
                    id: s.id,
                    text: neutralise(&s.text),
                })
                .collect(),
            kind_tiers: {
                let config = self.config.as_ref().map(watch::Receiver::borrow);
                KindRow::ALL
                    .iter()
                    .map(|&row| {
                        (
                            row,
                            config
                                .as_ref()
                                .map_or_else(|| row.default_tier(), |c| c.routing.kinds.tier(row)),
                        )
                    })
                    .collect()
            },
        }
    }

    fn project_fact(&self, p: &ProjectRow) -> ProjectFact {
        let path = self.tilde(&p.path);
        ProjectFact {
            name: neutralise(&p.name),
            path: neutralise(&path),
            delivery: p.delivery,
            autonomy: p.autonomy,
            verify: p.verification,
        }
    }

    /// `~/Developer/api-server` for a path under your home folder.
    fn tilde(&self, path: &str) -> String {
        let Some(home) = &self.home else {
            return path.to_owned();
        };
        let home = home.to_string_lossy();
        let home = home.trim_end_matches('/');
        if path == home {
            "~".to_owned()
        } else if let Some(rest) = path
            .strip_prefix(home)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            format!("~/{rest}")
        } else {
            path.to_owned()
        }
    }
}

impl ContextSource for ContextBuilder {
    fn context_block(
        &self,
        agent: AgentId,
        turn: TurnId,
    ) -> BoxFuture<'_, Result<String, SeatError>> {
        Box::pin(async move { Ok(block::render(&self.facts(agent, turn).await?)) })
    }
}

/// Everything read for one block, in one database call.
#[derive(Debug)]
struct Gathered {
    watermark: Option<Watermark>,
    projects: Vec<ProjectRow>,
    workers: Vec<WorkerRow>,
    tasks: Vec<TaskRow>,
    you: EventPage,
    activity: EventPage,
    model_changes: EventPage,
    outcomes: Vec<EventRow>,
    standing: Vec<(StandingInstructionId, String)>,
    tiers: Vec<TierRow>,
    user_message_sources: Vec<qm_store::user_message_sources::UserMessageSource>,
}

fn gather(conn: &mut Connection, agent: AgentId, turn: TurnId) -> Result<Gathered, StoreError> {
    // One read transaction, so every section describes the same moment.
    let tx = conn.transaction()?;
    let range = context::range(&tx, agent, turn)?;
    let (after, before) = (range.after, range.before);
    let gathered = Gathered {
        watermark: range.watermark,
        projects: context::projects(&tx)?,
        workers: context::workers(&tx)?,
        tasks: context::open_tasks(&tx)?,
        you: events::since(&tx, after, before, YOU, SINCE_LINES)?,
        activity: events::since(&tx, after, before, ACTIVITY, SINCE_LINES)?,
        model_changes: events::since(&tx, after, before, MODEL_CHANGES, SINCE_LINES)?,
        outcomes: events::latest(&tx, OUTCOMES, RECENT_OUTCOMES)?,
        standing: standing_instructions::active(&tx)?,
        tiers: context::tier_models(&tx)?,
        user_message_sources: qm_store::user_message_sources::recent(&tx, agent)?,
    };
    tx.commit()?;
    Ok(gathered)
}

fn worker_fact(w: &WorkerRow, now: Timestamp) -> WorkerFact {
    match w {
        WorkerRow::Crewmate(c) => WorkerFact::Crewmate(crewmate_fact(c, now)),
        WorkerRow::Secondmate(s) => WorkerFact::Secondmate(SecondmateFact {
            id: s.id,
            scope: neutralise(&s.scope),
            status: s.status.to_string(),
            base_branch: s.base_branch.as_deref().map(neutralise),
        }),
    }
}

fn crewmate_fact(c: &CrewmateRow, now: Timestamp) -> CrewmateFact {
    let minutes = u64::try_from(now.duration_since(c.since).as_secs() / 60).unwrap_or(0);
    let note = match (c.kind, c.builds_on) {
        (TaskKind::Review, Some(reviewed)) => Some(format!("reviews {reviewed}")),
        _ => None,
    };
    CrewmateFact {
        task: c.task,
        under: c.secondmate,
        project: neutralise(&c.project),
        kind: c.kind,
        status: block::status_words(c.status).to_owned(),
        minutes,
        effort: c.effort,
        model: c.model.as_ref().map(|m| neutralise(m.as_str())),
        note,
    }
}

fn task_fact(t: &TaskRow) -> TaskFact {
    // The one-line state is enriched elsewhere with PRs, checks, holds and reports.
    let under = match (t.status, t.secondmate) {
        (TaskStatus::Running, Some(sm)) => Some(format!("running under {sm}")),
        _ => None,
    };
    // Verification's and delivery's line.
    let detail = match (under, t.detail.as_deref().map(neutralise)) {
        (Some(u), Some(d)) => Some(format!("{u} · {d}")),
        (u, d) => u.or(d),
    };
    TaskFact {
        task: t.task,
        label: t.label,
        project: neutralise(&t.project),
        title: neutralise(&t.title),
        status: t.status,
        detail,
    }
}

fn tier_models(t: &TierRow) -> TierModels {
    TierModels {
        tier: t.tier,
        models: t
            .models
            .iter()
            .map(|m| {
                let effort = m.effort.map_or("default/none", |e| e.as_str());
                format!(
                    "{} (provider={} model={} effort={effort})",
                    neutralise(m.canonical.as_str()),
                    m.provider,
                    neutralise(m.model.as_str()),
                )
            })
            .collect(),
        note: (t.tier == Tier::Premium).then(|| PREMIUM_NOTE.to_owned()),
    }
}
