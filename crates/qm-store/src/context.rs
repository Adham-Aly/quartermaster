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
use crate::time::from_millis;
use jiff::Timestamp;
use qm_types::domain::{
    AgentStatus, Autonomy, DeliveryMode, Effort, ProjectOrigin, TaskKind, TaskStatus, Tier,
    VerificationLevel,
};
use qm_types::ids::{AgentId, ModelId, SecondmateId, TaskId, TurnId};
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
         ORDER BY origin = 'workspace', seq",
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
         ORDER BY t.seq",
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
         ORDER BY s.seq",
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
         ORDER BY t.seq DESC",
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

/// Where "since your last turn" starts: the turn the block last showed everything up to, and the
/// creation order (`seq`) of its `seat.turn_started` event. A `seq` rather than an event ID, so it
/// still means something after a restart, when the turn itself is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watermark {
    pub turn: TurnId,
    pub seq: i64,
}

/// The events a turn's block shows as "since": those with `after < seq`, and `seq < before` when
/// there is an upper bound. Both come from `block_watermarks`, never from the events table, so a
/// reload reads the same range (the kept predicate keeps only some kinds, and never a turn's own
/// `seat.turn_started`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub watermark: Option<Watermark>,
    pub after: i64,
    pub before: Option<i64>,
}

/// The range for `turn`. While `turn` is the newest turn the row names, the lower bound is the
/// turn before it (none for a first turn, so everything from the start) and the upper bound is
/// `turn`'s own start. A turn the row doesn't name has no upper bound and starts after the row's
/// turn. A turn that ended `no_wakes` was rolled back by [`crate::turns::finish_turn`], so it
/// never shows as the watermark.
pub fn range(conn: &Connection, agent: AgentId, turn: TurnId) -> Result<Range, StoreError> {
    // (turn, its start, the turn before it, that turn's start)
    type Row = (Option<TurnId>, i64, Option<TurnId>, Option<i64>);
    let row: Option<Row> = conn
        .query_row(
            "SELECT turn_id, event_seq, prev_turn_id, prev_event_seq
             FROM block_watermarks WHERE agent_id = ?1",
            params![agent],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Some(newest), start, _, _)) if newest != turn => Range {
            watermark: Some(Watermark {
                turn: newest,
                seq: start,
            }),
            after: start,
            before: None,
        },
        Some((Some(_), start, Some(previous), previous_start)) => {
            let after = previous_start.unwrap_or(0);
            Range {
                watermark: Some(Watermark {
                    turn: previous,
                    seq: after,
                }),
                after,
                before: Some(start),
            }
        }
        Some((Some(_), start, None, _)) => Range {
            watermark: None,
            after: 0,
            before: Some(start),
        },
        _ => Range {
            watermark: None,
            after: 0,
            before: None,
        },
    })
}

/// The watermark for `turn`: the turn the block last showed everything up to (see [`range`]).
pub fn watermark(
    conn: &Connection,
    agent: AgentId,
    turn: TurnId,
) -> Result<Option<Watermark>, StoreError> {
    Ok(range(conn, agent, turn)?.watermark)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::events;
    use crate::turns::{self, FinishedTurn, NO_WAKES, NewTurn, TurnTotals};
    use qm_types::domain::{StartedBy, TurnStatus};

    /// Starts a turn and ends it with `reason`, as a completed turn.
    fn finished(conn: &mut Connection, agent: AgentId, at: i64, reason: Option<&str>) -> TurnId {
        let at = Timestamp::from_second(at).expect("a time");
        let turn = turns::start_turn(
            conn,
            &NewTurn {
                agent,
                started_by: StartedBy::AppEvent,
                model: None,
                at,
            },
        )
        .expect("a turn");
        turns::finish_turn(
            conn,
            &FinishedTurn {
                turn,
                agent,
                status: TurnStatus::Completed,
                reason: reason.map(str::to_owned),
                detail: None,
                actions: Vec::new(),
                retried: false,
                usage: TurnTotals::default(),
                model: None,
                close_outs: Vec::new(),
                limit: None,
                release_wakes: false,
                at,
            },
        )
        .expect("the turn's end");
        turn
    }

    #[tokio::test]
    async fn the_watermark_skips_a_no_wakes_turn_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("qm-watermark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let path = dir.join("state.db");
        let agent = AgentId::new(1);

        let db = Db::open(&path).expect("a database");
        db.call(|conn| {
            conn.execute_batch(
                "INSERT INTO projects (id, name, path, origin, default_branch, delivery_mode, \
                     merge_method, created_at, seq) \
                 VALUES (1, 'p', '/p', 'created', 'main', 'direct_pr', 'merge', 1, 1);
                 INSERT INTO agents (id, role, home, status, created_at, seq) \
                 VALUES (1, 'quartermaster', '/home', 'idle', 1, 1);",
            )?;
            Ok(())
        })
        .await
        .expect("the rows");

        // A ends normally, B finds no wakes and never reaches the model, C ends normally.
        let (a, c, seq_a, seq_c, for_c) = db
            .call(move |conn| {
                let a = finished(conn, agent, 2, None);
                let _b = finished(conn, agent, 3, Some(NO_WAKES));
                let c = finished(conn, agent, 4, None);
                let seq_a = events::turn_started_seq(conn, a)?.expect("A's start");
                let seq_c = events::turn_started_seq(conn, c)?.expect("C's start");
                // While C is still the newest turn, its watermark is A.
                let for_c = watermark(conn, agent, c)?;
                Ok((a, c, seq_a, seq_c, for_c))
            })
            .await
            .expect("three turns");
        assert_eq!(
            for_c,
            Some(Watermark {
                turn: a,
                seq: seq_a
            }),
            "B never reached the model, so C's watermark is A"
        );

        // N starts: its watermark is C.
        let (n, seq_n, for_n) = db
            .call(move |conn| {
                let n = finished(conn, agent, 5, None);
                let seq_n = events::turn_started_seq(conn, n)?.expect("N's start");
                Ok((n, seq_n, watermark(conn, agent, n)?))
            })
            .await
            .expect("N");
        assert_eq!(
            for_n,
            Some(Watermark {
                turn: c,
                seq: seq_c
            })
        );
        drop(db);

        // After a restart the turns are gone from memory, but the row still names N: the next
        // turn's watermark is still N.
        let db = Db::open(&path).expect("a restart");
        let after_restart = db
            .call(move |conn| {
                let m = finished(conn, agent, 6, None);
                watermark(conn, agent, m)
            })
            .await
            .expect("the watermark after a restart");
        assert_eq!(
            after_restart,
            Some(Watermark {
                turn: n,
                seq: seq_n
            }),
            "the watermark survives a restart"
        );
        drop(db);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
