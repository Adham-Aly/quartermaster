//! A secondmate's context block.
//!
//! The same block as the Quartermaster's, rendered by `qm_prompts::context_block::render`, filled
//! from the secondmate's slice of the world: the projects in its scope (every project when its
//! list is empty), its own crewmates and open tasks, the activity on its tasks since its last
//! turn, its open requests, and no standing instructions (those are the Quartermaster's).

use crate::context_block::{ACTIVITY, ContextBuilder, OUTCOMES};
use crate::turn_io::{ContextSource, SeatError};
use jiff::tz::TimeZone;
use qm_policy::markers::neutralise;
use qm_prompts::context_block::{
    self as block, EventLine, RECENT_OUTCOMES, SINCE_LINES, Since, WorkerFact,
};
use qm_store::db::Db;
use qm_store::events;
use qm_store::secondmates;
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::ids::{AgentId, SecondmateId, TaskId, TurnId};
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use tokio::sync::watch;

/// How much of a request each line shows.
const REQUEST_LINE_CHARS: usize = 80;
/// How many events are read before keeping only those on its tasks.
const ACTIVITY_READ: usize = 500;

/// Builds one secondmate's context block each turn.
pub struct SecondmateContext {
    db: Db,
    id: SecondmateId,
    builder: ContextBuilder,
    tz: TimeZone,
}

impl fmt::Debug for SecondmateContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecondmateContext")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What the secondmate's slice adds to the shared facts.
struct Slice {
    scope_projects: Option<HashSet<String>>,
    own_tasks: HashSet<TaskId>,
    activity: events::EventPage,
    requests: Vec<secondmates::RequestRow>,
    /// The newest outcomes on its own tasks, newest first.
    outcomes: Vec<events::EventRow>,
}

impl SecondmateContext {
    pub fn new(db: Db, clock: &Arc<dyn Clock>, id: SecondmateId) -> Self {
        let builder = ContextBuilder::new(db.clone(), Arc::clone(clock));
        Self {
            db,
            id,
            builder,
            tz: TimeZone::system(),
        }
    }

    /// Share the same live Task kinds settings as the Quartermaster.
    #[must_use]
    pub fn with_config(mut self, config: watch::Receiver<Config>) -> Self {
        self.builder = self.builder.with_config(config);
        self
    }

    async fn slice(&self, agent: AgentId, turn: TurnId) -> Result<Slice, SeatError> {
        let id = self.id;
        Ok(self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let listed = secondmates::projects_of(&tx, id)?;
                let scope_projects = (!listed.is_empty())
                    .then(|| listed.into_iter().map(|(_, n)| n.to_lowercase()).collect());
                let own_tasks: HashSet<TaskId> =
                    secondmates::tasks_of(&tx, agent)?.into_iter().collect();
                let range = qm_store::context::range(&tx, agent, turn)?;
                let mut page =
                    events::since(&tx, range.after, range.before, ACTIVITY, ACTIVITY_READ)?;
                page.rows.retain(|e| supervised_by(e, agent));
                let more = page.rows.len().saturating_sub(SINCE_LINES);
                page.rows.truncate(SINCE_LINES);
                page.more = more;
                let requests = secondmates::open_requests(&tx, id)?;
                let outcomes = events::latest_of(&tx, OUTCOMES, agent, RECENT_OUTCOMES)?;
                tx.commit()?;
                Ok(Slice {
                    scope_projects,
                    own_tasks,
                    activity: page,
                    requests,
                    outcomes,
                })
            })
            .await?)
    }
}

/// Whether `event` belongs to `agent`'s tasks: a task event names its supervisor in its payload.
fn supervised_by(event: &events::EventRow, agent: AgentId) -> bool {
    event
        .payload
        .get("supervisor")
        .and_then(serde_json::Value::as_i64)
        == Some(agent.get())
}

fn cut(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut s: String = one_line.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

impl ContextSource for SecondmateContext {
    fn context_block(
        &self,
        agent: AgentId,
        turn: TurnId,
    ) -> BoxFuture<'_, Result<String, SeatError>> {
        Box::pin(async move {
            let mut facts = self.builder.facts(agent, turn).await?;
            let slice = self.slice(agent, turn).await?;
            let id = self.id;

            if let Some(scope) = &slice.scope_projects {
                facts
                    .projects
                    .retain(|p| scope.contains(&p.name.to_lowercase()));
            }
            facts.workers.retain_mut(|w| match w {
                WorkerFact::Crewmate(c) if c.under == Some(id) => {
                    c.under = None;
                    true
                }
                _ => false,
            });
            facts.workers_total = facts.workers.len();
            facts.tasks.retain(|t| slice.own_tasks.contains(&t.task));
            for t in &mut facts.tasks {
                t.detail = None;
            }
            facts.tasks_open = facts.tasks.len();
            let line = |e: &events::EventRow| EventLine {
                at: e.at.to_zoned(self.tz.clone()),
                text: neutralise(&e.summary),
            };
            facts.activity = Since {
                lines: slice.activity.rows.iter().map(line).collect(),
                more: slice.activity.more,
            };
            // `since`: one line per open or answered request; your own words to it arrive as the
            // turn's message.
            facts.you = Since {
                lines: slice
                    .requests
                    .iter()
                    .map(|r| EventLine {
                        at: r.created_at.to_zoned(self.tz.clone()),
                        text: neutralise(&format!(
                            "{} {}: {}",
                            r.id,
                            r.status,
                            cut(&r.request, REQUEST_LINE_CHARS)
                        )),
                    })
                    .collect(),
                more: 0,
            };
            // `recent_outcomes`: the seat's outcome events, restricted to its own tasks; each
            // outcome event names its task.
            facts.recent_outcomes = slice.outcomes.iter().map(line).collect();
            facts.standing.clear();
            Ok(block::render(&facts))
        })
    }
}
