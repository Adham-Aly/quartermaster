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
    RECENT_OUTCOMES, SINCE_LINES, SecondmateFact, Since, TaskFact, TierModels, WorkerFact,
};
use qm_store::StoreError;
use qm_store::context::{self, CrewmateRow, ProjectRow, TaskRow, TierRow, Watermark, WorkerRow};
use qm_store::db::Db;
use qm_store::events::{self, EventPage, EventRow};
use qm_store::standing_instructions;
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::{ProjectOrigin, TaskKind, TaskStatus, Tier};
use qm_types::ids::{AgentId, StandingInstructionId, TurnId};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;

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
}

impl ContextBuilder {
    /// Times in the system's time zone; paths under your home folder shown with `~`.
    pub fn new(db: Db, clock: Arc<dyn Clock>) -> Self {
        Self {
            db,
            clock,
            tz: TimeZone::system(),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_absolute()),
        }
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
        }
    }

    fn project_fact(&self, p: &ProjectRow) -> ProjectFact {
        let path = if p.origin == ProjectOrigin::Workspace {
            format!("{} (personal workspace)", self.tilde(&p.path))
        } else {
            self.tilde(&p.path)
        };
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
}

fn gather(conn: &mut Connection, agent: AgentId, turn: TurnId) -> Result<Gathered, StoreError> {
    // One read transaction, so every section describes the same moment.
    let tx = conn.transaction()?;
    let watermark = context::watermark(&tx, agent, turn)?;
    let after = watermark.map_or(0, |w| w.seq);
    let before = events::turn_started_seq(&tx, turn)?;
    let gathered = Gathered {
        watermark,
        projects: context::projects(&tx)?,
        workers: context::workers(&tx)?,
        tasks: context::open_tasks(&tx)?,
        you: events::since(&tx, after, before, YOU, SINCE_LINES)?,
        activity: events::since(&tx, after, before, ACTIVITY, SINCE_LINES)?,
        model_changes: events::since(&tx, after, before, MODEL_CHANGES, SINCE_LINES)?,
        outcomes: events::latest_of(&tx, OUTCOMES, agent, RECENT_OUTCOMES)?,
        standing: standing_instructions::active(&tx)?,
        tiers: context::tier_models(&tx)?,
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
        project: neutralise(&t.project),
        title: neutralise(&t.title),
        status: t.status,
        detail,
    }
}

fn tier_models(t: &TierRow) -> TierModels {
    TierModels {
        tier: t.tier,
        models: t.models.iter().map(|m| neutralise(m.as_str())).collect(),
        note: (t.tier == Tier::Premium).then(|| PREMIUM_NOTE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_io::ContextSource;
    use qm_store::turns::{self, FinishedTurn, NewTurn, TurnTotals};
    use qm_types::clock::Sleep;
    use qm_types::domain::{StartedBy, TurnStatus};
    use std::time::Instant as StdInstant;

    /// A clock that stands still, so a render of the same state is the same bytes.
    #[derive(Debug)]
    struct Still(Timestamp);

    impl Clock for Still {
        fn now(&self) -> Timestamp {
            self.0
        }

        fn instant(&self) -> StdInstant {
            StdInstant::now()
        }

        fn sleep_until(&self, _deadline: StdInstant) -> Sleep {
            Box::pin(std::future::ready(()))
        }
    }

    /// The rows every block needs: a project and the Quartermaster.
    const SETUP: &str =
        "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
             merge_method, created_at, seq) \
         VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
         INSERT INTO agents (id, role, home, status, created_at, seq) \
         VALUES (1, 'quartermaster', '/home', 'idle', 1, 1);";

    /// The creation order for an event or row made at `ms` milliseconds.
    fn seq_at(ms: i64) -> i64 {
        qm_store::seq::next(Timestamp::from_millisecond(ms).expect("a time"))
    }

    fn begin(conn: &mut Connection, agent: AgentId, second: i64) -> Result<TurnId, StoreError> {
        let at = Timestamp::from_second(second).expect("a time");
        turns::start_turn(
            conn,
            &NewTurn {
                agent,
                started_by: StartedBy::AppEvent,
                model: None,
                at,
            },
        )
    }

    fn end(
        conn: &mut Connection,
        agent: AgentId,
        turn: TurnId,
        second: i64,
    ) -> Result<(), StoreError> {
        turns::finish_turn(
            conn,
            &FinishedTurn {
                turn,
                agent,
                status: TurnStatus::Completed,
                reason: None,
                detail: None,
                actions: Vec::new(),
                retried: false,
                usage: TurnTotals::default(),
                model: None,
                close_outs: Vec::new(),
                limit: None,
                release_wakes: false,
                at: Timestamp::from_second(second).expect("a time"),
            },
        )
    }

    #[tokio::test]
    async fn the_block_reads_the_same_across_a_restart_and_each_line_shows_once() {
        let dir = std::env::temp_dir().join(format!("qm-block-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("state.db");
        let clock: Arc<dyn Clock> = Arc::new(Still(
            Timestamp::from_second(1_800_000_000).expect("a time"),
        ));
        let agent = AgentId::new(1);

        let db = Db::open(&path).expect("a database");
        db.call(|conn| {
            conn.execute_batch(SETUP)?;
            // Two open tasks, the older one first: the block lists them in creation order.
            for (id, title, ms) in [(1, "OLD-TASK-MARK", 10), (2, "NEW-TASK-MARK", 11)] {
                conn.execute(
                    "INSERT INTO tasks (id, project_id, supervisor_id, kind, title, goal, \
                         delivery_mode, branch, status, created_at, seq) \
                     VALUES (?1, 1, 1, 'build', ?2, 'g', 'direct_pr', ?3, 'running', 1, ?4)",
                    rusqlite::params![id, title, format!("qm/task-{id}"), seq_at(ms)],
                )?;
            }
            Ok(())
        })
        .await
        .expect("the rows");

        // Turn A ends; a line arrives between turns; turn B starts and ends after it.
        db.call(move |conn| {
            let a = begin(conn, agent, 2)?;
            end(conn, agent, a, 3)?;
            conn.execute(
                "INSERT INTO events (at, kind, summary, payload, seq) \
                 VALUES (4000, 'you.notice', 'LINE-MARK', '{}', ?1)",
                [seq_at(4000)],
            )?;
            Ok(())
        })
        .await
        .expect("turn A and a line");
        let turn_b = db
            .call(move |conn| begin(conn, agent, 5))
            .await
            .expect("turn B");
        let builder = ContextBuilder::new(db.clone(), Arc::clone(&clock));
        let before = builder
            .context_block(agent, turn_b)
            .await
            .expect("the block");
        assert!(
            before.contains("LINE-MARK"),
            "a line since the last turn is shown"
        );
        let old = before.find("OLD-TASK-MARK").expect("the old task");
        let new = before.find("NEW-TASK-MARK").expect("the new task");
        assert!(old < new, "tasks read in creation order");
        db.call(move |conn| end(conn, agent, turn_b, 6))
            .await
            .expect("turn B ends");
        drop(builder);
        drop(db);

        // After a restart, the same turn renders the same bytes.
        let db = Db::open(&path).expect("a restart");
        let builder = ContextBuilder::new(db.clone(), Arc::clone(&clock));
        let after = builder
            .context_block(agent, turn_b)
            .await
            .expect("the block");
        assert_eq!(
            after, before,
            "the block is byte-identical across a restart"
        );

        // The next turn's block starts after the line: it shows once.
        let turn_c = db
            .call(move |conn| begin(conn, agent, 7))
            .await
            .expect("turn C");
        let next = builder
            .context_block(agent, turn_c)
            .await
            .expect("the block");
        assert!(!next.contains("LINE-MARK"), "a line is shown once");
        drop(builder);
        drop(db);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
