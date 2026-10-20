//! Read-only queries for the Quartermaster's context block.
//!
//! The block is rebuilt in full every turn from these, so it is always current whatever the
//! session remembers. Each function reads; none writes. The core (`qm_core::context_block`) runs
//! them in one database call and turns the rows into the prompt's facts.
//!
//! | Section | Source |
//! |---|---|
//! | `projects` | [`projects`]: `projects` not removed |
//! | `workers` | [`workers`]: `agents` not retired, other than the Quartermaster |
//! | `tasks` | [`open_tasks`]: `tasks` not closed, newest first |
//! | since-sections | `events` after the [`watermark`] (`crate::events::since`) |
//! | `standing_instructions` | `crate::standing_instructions::active` |
//! | `models` | [`tier_models`]: the routing pool, by tier |

use crate::StoreError;
use crate::events::TURN_STARTED;
use crate::time::from_millis;
use jiff::Timestamp;
use qm_types::domain::{
    AgentStatus, Autonomy, DeliveryMode, Effort, ProjectOrigin, TaskKind, TaskStatus, Tier,
    VerificationLevel,
};
use qm_types::ids::{AgentId, EventId, ModelId, SecondmateId, TaskId, TurnId};
use rusqlite::{Connection, OptionalExtension, params};

/// A registered project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    pub name: String,
    pub path: String,
    pub origin: ProjectOrigin,
    pub delivery: DeliveryMode,
    pub autonomy: Autonomy,
    pub verification: VerificationLevel,
}

/// Every project not removed: registered ones in the order they were added, the built-in
/// workspace last.
pub fn projects(conn: &Connection) -> Result<Vec<ProjectRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT name, path, origin, delivery_mode, autonomy, verification_level FROM projects
         WHERE removed_at IS NULL
         ORDER BY origin = 'workspace', id",
    )?;
    Ok(stmt
        .query_map([], |r| {
            Ok(ProjectRow {
                name: r.get(0)?,
                path: r.get(1)?,
                origin: r.get(2)?,
                delivery: r.get(3)?,
                autonomy: r.get(4)?,
                verification: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A crewmate at work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrewmateRow {
    pub agent: AgentId,
    pub task: TaskId,
    pub project: String,
    pub kind: TaskKind,
    pub status: TaskStatus,
    /// When its task started (or, before that, when the crewmate was created).
    pub since: Timestamp,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// The task it builds on: the task a review reviews.
    pub builds_on: Option<TaskId>,
    /// The secondmate it works under, if any.
    pub secondmate: Option<SecondmateId>,
}

/// A secondmate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondmateRow {
    pub agent: AgentId,
    pub id: SecondmateId,
    pub scope: String,
    pub status: AgentStatus,
    pub base_branch: Option<String>,
}

/// A worker, as the `<workers>` section lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerRow {
    Crewmate(CrewmateRow),
    Secondmate(SecondmateRow),
}

/// Every working agent other than the Quartermaster, in tree order: the Quartermaster's own
/// crewmates by task, then each secondmate followed by its crewmates.
pub fn workers(conn: &Connection) -> Result<Vec<WorkerRow>, StoreError> {
    let mut crew_stmt = conn.prepare(
        "SELECT a.id, t.id, p.name, t.kind, t.status, coalesce(t.started_at, a.created_at),
                a.model, a.effort, t.builds_on, s.secondmate_id
         FROM agents AS a
         JOIN tasks AS t ON t.id = a.task_id
         JOIN projects AS p ON p.id = t.project_id
         LEFT JOIN agents AS s ON s.id = a.supervisor_id AND s.role = 'secondmate'
         WHERE a.role = 'crewmate' AND a.retired_at IS NULL
         ORDER BY t.id",
    )?;
    let raw = crew_stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, AgentId>(0)?,
                r.get::<_, TaskId>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, TaskKind>(3)?,
                r.get::<_, TaskStatus>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<ModelId>>(6)?,
                r.get::<_, Option<Effort>>(7)?,
                r.get::<_, Option<TaskId>>(8)?,
                r.get::<_, Option<SecondmateId>>(9)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut crew = Vec::with_capacity(raw.len());
    for (agent, task, project, kind, status, since, model, effort, builds_on, secondmate) in raw {
        crew.push(CrewmateRow {
            agent,
            task,
            project,
            kind,
            status,
            since: from_millis("tasks.started_at", since)?,
            model,
            effort,
            builds_on,
            secondmate,
        });
    }

    let mut sm_stmt = conn.prepare(
        "SELECT a.id, s.id, s.scope, a.status, s.standing_base_branch
         FROM agents AS a JOIN secondmates AS s ON s.id = a.secondmate_id
         WHERE a.role = 'secondmate' AND a.retired_at IS NULL AND s.retired_at IS NULL
         ORDER BY s.id",
    )?;
    let secondmates = sm_stmt
        .query_map([], |r| {
            Ok(SecondmateRow {
                agent: r.get(0)?,
                id: r.get(1)?,
                scope: r.get(2)?,
                status: r.get(3)?,
                base_branch: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out: Vec<WorkerRow> = crew
        .iter()
        .filter(|c| c.secondmate.is_none())
        .cloned()
        .map(WorkerRow::Crewmate)
        .collect();
    for sm in secondmates {
        let id = sm.id;
        out.push(WorkerRow::Secondmate(sm));
        out.extend(
            crew.iter()
                .filter(|c| c.secondmate == Some(id))
                .cloned()
                .map(WorkerRow::Crewmate),
        );
    }
    Ok(out)
}

/// An open or active task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRow {
    pub task: TaskId,
    pub project: String,
    pub kind: TaskKind,
    pub title: String,
    pub status: TaskStatus,
    /// The secondmate whose crewmate runs it, if any.
    pub secondmate: Option<SecondmateId>,
    pub builds_on: Option<TaskId>,
    /// The one-line state verification and delivery know: verification's line, then delivery's,
    /// joined with " · "; `None` when neither says anything.
    pub detail: Option<String>,
}

/// A task's verification and delivery lines, joined.
pub fn task_detail(conn: &Connection, task: TaskId) -> Result<Option<String>, StoreError> {
    let parts: Vec<String> = [
        crate::verification::task_line(conn, task)?,
        crate::delivery::task_line(conn, task)?,
    ]
    .into_iter()
    .flatten()
    .collect();
    Ok((!parts.is_empty()).then(|| parts.join(" · ")))
}

/// Every task not closed, newest first.
pub fn open_tasks(conn: &Connection) -> Result<Vec<TaskRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, p.name, t.kind, t.title, t.status, s.secondmate_id, t.builds_on
         FROM tasks AS t
         JOIN projects AS p ON p.id = t.project_id
         LEFT JOIN agents AS s ON s.id = t.supervisor_id AND s.role = 'secondmate'
         WHERE t.status <> 'closed'
         ORDER BY t.id DESC",
    )?;
    let mut rows = stmt
        .query_map([], |r| {
            Ok(TaskRow {
                task: r.get(0)?,
                project: r.get(1)?,
                kind: r.get(2)?,
                title: r.get(3)?,
                status: r.get(4)?,
                secondmate: r.get(5)?,
                builds_on: r.get(6)?,
                detail: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in &mut rows {
        row.detail = task_detail(conn, row.task)?;
    }
    Ok(rows)
}

/// Where "since your last turn" starts: the previous turn of the agent and its `seat.turn_started`
/// event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watermark {
    pub turn: TurnId,
    pub event: EventId,
}

/// The agent's turn before `turn` that reached the model, with its `seat.turn_started` event. A
/// wake turn that found no wakes never reached the model (`no_wakes`), so it is skipped: what
/// happened before it still counts as new.
pub fn watermark(
    conn: &Connection,
    agent: AgentId,
    turn: TurnId,
) -> Result<Option<Watermark>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT e.turn_id, e.id FROM events AS e JOIN turns AS t ON t.id = e.turn_id
             WHERE e.kind = ?1 AND t.agent_id = ?2 AND t.id < ?3
               AND coalesce(t.outcome_reason, '') <> 'no_wakes'
             ORDER BY e.id DESC LIMIT 1",
            params![TURN_STARTED, agent, turn],
            |r| {
                Ok(Watermark {
                    turn: r.get(0)?,
                    event: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// The models in one tier, as routing can use them now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierRow {
    pub tier: Tier,
    /// Canonical model ids, in placement order, each once.
    pub models: Vec<ModelId>,
}

/// The routing pool by tier, strongest first; tiers with nothing usable are left out.
pub fn tier_models(conn: &Connection) -> Result<Vec<TierRow>, StoreError> {
    let pool = crate::models::pool(conn)?;
    let mut out = Vec::new();
    for tier in [Tier::Premium, Tier::High, Tier::Medium, Tier::Low] {
        let mut models: Vec<ModelId> = Vec::new();
        for row in pool.iter().filter(|r| r.tier == Some(tier)) {
            if !models.contains(&row.facts.canonical) {
                models.push(row.facts.canonical.clone());
            }
        }
        if !models.is_empty() {
            out.push(TierRow { tier, models });
        }
    }
    Ok(out)
}
