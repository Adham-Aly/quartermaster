//! Read queries for the crewmate's brief and for the crew and task panes.
//!
//! Everything here only reads, straight from the schema (`tasks`, `projects`, `agents`,
//! `secondmates`, `secondmate_requests`, `reports`, `turns`, `routing_decisions`, `accounts`,
//! `held_actions`, `events`), so the core's brief writer and the crew view depend on nothing but
//! the tables. The core turns these rows into the brief's facts and the panes' display rows.

use crate::StoreError;
use crate::messages;
use crate::time::from_millis;
use jiff::Timestamp;
use qm_types::domain::{
    AgentStatus, DeliveryMode, Effort, ReportOutcome, ReportSource, ReviewVerdict, Role, TaskKind,
    TaskStatus, Tier, VerificationLevel, VerificationSource,
};
use qm_types::ids::{AgentId, CommitSha, SecondmateId, TaskId, TaskLabelId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::collections::BTreeMap;

/// `task.tool`: a tool call of a crewmate's, the task pane's activity.
pub const TASK_TOOL_EVENT: &str = "task.tool";

/// How many tool calls the task pane shows.
pub const ACTIVITY_ROWS: usize = 5;

/// How far the app follows retries and builds-on links looking for the user's words.
const WORDS_DEPTH: usize = 8;

/// One constraint as `tasks.constraints` stores it (`start_crewmate`'s `Constraint`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct StoredConstraint {
    pub text: String,
    #[serde(default)]
    pub freeze: Vec<String>,
}

/// Who supervises a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supervisor {
    pub agent: AgentId,
    pub role: Role,
    /// Set when the supervisor is a secondmate.
    pub secondmate: Option<SecondmateId>,
}

/// Everything the brief of one task is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefInputs {
    pub task: TaskId,
    /// The task (`T-`) the crewmate belongs to.
    pub label: TaskLabelId,
    pub kind: TaskKind,
    pub supervisor: Supervisor,
    pub project: String,
    pub default_branch: String,
    /// The project has a GitHub remote (plain folders and local-only projects don't).
    pub has_remote: bool,
    pub delivery: DeliveryMode,
    /// The project's own delivery: anything but a plain folder means its folder is a repository,
    /// even when this task works in it in place.
    pub project_delivery: DeliveryMode,
    /// The task's level and where it came from; `None` for tasks with no level.
    pub verification: Option<(VerificationLevel, VerificationSource)>,
    /// The base branch typed for this task, if one was.
    pub base_branch: Option<String>,
    /// The settled start: `origin/main`, or for a review the reviewed branch.
    pub base_ref: Option<String>,
    pub base_commit: Option<CommitSha>,
    /// The task's own branch, absent for folder tasks.
    pub branch: Option<String>,
    pub port_base: Option<u16>,
    pub goal: String,
    /// The user's own words, resolved (see [`resolved_words`]).
    pub user_words: String,
    /// The supervisor left passages about other work out of them.
    pub user_words_trimmed: bool,
    pub instructions: Vec<String>,
    pub constraints: Vec<StoredConstraint>,
    pub tests_required: bool,
    pub builds_on: Option<TaskId>,
    pub retry_of: Option<TaskId>,
    /// The project's folder: the task's worktree's checkout, or for a folder task the folder it
    /// works in.
    pub folder: String,
}

/// One report, as the brief reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReport {
    pub source: ReportSource,
    pub outcome: Option<ReportOutcome>,
    /// The five headings, as stored (`intent`, `what_changed`, `verified`, `risk`, `not_done`).
    pub headings: BTreeMap<String, String>,
    pub answer: Option<String>,
    pub reproduced: Option<bool>,
    pub diagnosis: bool,
    pub review_verdict: Option<ReviewVerdict>,
    pub head_commit: Option<CommitSha>,
    pub final_text: Option<String>,
}

/// A task another one builds on or retries, with its latest reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorTask {
    pub task: TaskId,
    pub kind: TaskKind,
    pub status: TaskStatus,
    /// The task's branch, absent for folder tasks.
    pub branch: Option<String>,
    /// Where its branch started (`tasks.base_commit`).
    pub base_commit: Option<CommitSha>,
    pub head_commit: Option<CommitSha>,
    /// Its project's folder, the repository its branch lives in.
    pub project_path: String,
    /// Its latest clear report (a `report` call).
    pub last_clear: Option<StoredReport>,
    /// Its latest report of any kind (a clear one, or final text kept as one).
    pub last_any: Option<StoredReport>,
}

/// The brief's inputs for `task`; `None` if there is no such task.
pub fn brief_inputs(conn: &Connection, task: TaskId) -> Result<Option<BriefInputs>, StoreError> {
    let row = conn
        .query_row(
            "SELECT t.kind, t.supervisor_id, a.role, a.secondmate_id, p.name, p.default_branch,
                    p.github_remote IS NOT NULL, t.delivery_mode, t.verification_level,
                    t.verification_source, t.base_branch, t.base_ref, t.base_commit, t.port_base,
                    t.goal, t.instructions, t.constraints, t.tests_required, t.builds_on,
                    t.retry_of, p.path, t.branch, p.delivery_mode, COALESCE(t.label_id, t.id)
             FROM tasks AS t
             JOIN projects AS p ON p.id = t.project_id
             JOIN agents AS a ON a.id = t.supervisor_id
             WHERE t.id = ?1",
            params![task],
            |r| {
                Ok(RawBrief {
                    kind: r.get(0)?,
                    supervisor: Supervisor {
                        agent: r.get(1)?,
                        role: r.get(2)?,
                        secondmate: r.get(3)?,
                    },
                    project: r.get(4)?,
                    default_branch: r.get(5)?,
                    has_remote: r.get(6)?,
                    delivery: r.get(7)?,
                    level: r.get(8)?,
                    source: r.get(9)?,
                    base_branch: r.get(10)?,
                    base_ref: r.get(11)?,
                    base_commit: r.get(12)?,
                    port_base: r.get(13)?,
                    goal: r.get(14)?,
                    instructions: r.get(15)?,
                    constraints: r.get(16)?,
                    tests_required: r.get(17)?,
                    builds_on: r.get(18)?,
                    retry_of: r.get(19)?,
                    path: r.get(20)?,
                    branch: r.get(21)?,
                    project_delivery: r.get(22)?,
                    label: r.get(23)?,
                })
            },
        )
        .optional()?;
    let Some(raw) = row else {
        return Ok(None);
    };
    let port_base = raw
        .port_base
        .map(|p| {
            u16::try_from(p).map_err(|_| StoreError::Invalid {
                column: "tasks.port_base",
                message: format!("{p} is not a port"),
            })
        })
        .transpose()?;
    let instructions: Vec<String> =
        serde_json::from_str(&raw.instructions).map_err(|source| StoreError::Json {
            column: "tasks.instructions",
            source,
        })?;
    let constraints: Vec<StoredConstraint> =
        serde_json::from_str(&raw.constraints).map_err(|source| StoreError::Json {
            column: "tasks.constraints",
            source,
        })?;
    let words = resolved_words(conn, task)?;
    Ok(Some(BriefInputs {
        task,
        label: raw.label,
        kind: raw.kind,
        supervisor: raw.supervisor,
        project: raw.project,
        default_branch: raw.default_branch,
        has_remote: raw.has_remote,
        delivery: raw.delivery,
        project_delivery: raw.project_delivery,
        verification: raw.level.zip(raw.source),
        base_branch: raw.base_branch,
        base_ref: raw.base_ref,
        base_commit: raw.base_commit,
        branch: raw.branch,
        port_base,
        goal: raw.goal,
        user_words: words.text,
        user_words_trimmed: !words.cuts.is_empty(),
        instructions,
        constraints,
        tests_required: raw.tests_required,
        builds_on: raw.builds_on,
        retry_of: raw.retry_of,
        folder: raw.path,
    }))
}

struct RawBrief {
    label: TaskLabelId,
    kind: TaskKind,
    supervisor: Supervisor,
    project: String,
    default_branch: String,
    has_remote: bool,
    delivery: DeliveryMode,
    level: Option<VerificationLevel>,
    source: Option<VerificationSource>,
    base_branch: Option<String>,
    base_ref: Option<String>,
    base_commit: Option<CommitSha>,
    port_base: Option<i64>,
    goal: String,
    instructions: String,
    constraints: String,
    tests_required: bool,
    builds_on: Option<TaskId>,
    retry_of: Option<TaskId>,
    path: String,
    branch: Option<String>,
    project_delivery: DeliveryMode,
}

/// The user's own words for a task, as its brief carries them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserWords {
    pub text: String,
    /// The passages its supervisor left out of the message as being about other work; empty when
    /// `text` is the message as it was.
    pub cuts: Vec<String>,
}

/// The user's own words for `task`, copied by the app (see [`resolved_words`]).
pub fn user_words(conn: &Connection, task: TaskId) -> Result<String, StoreError> {
    Ok(resolved_words(conn, task)?.text)
}

/// The user's own words for `task`, copied by the app: the trimmed words when its supervisor left
/// passages out, else the stored message's text when the task names one, else the words stored on
/// the row. A task started by an app event has none of its own; it inherits them from the task it
/// retries, else the task it builds on (when the same supervisor started both), else the
/// secondmate request it was routed from, else they're empty.
pub fn resolved_words(conn: &Connection, task: TaskId) -> Result<UserWords, StoreError> {
    let mut current = task;
    for _ in 0..WORDS_DEPTH {
        let row = conn
            .query_row(
                "SELECT t.user_words, t.user_words_message_id, t.retry_of, t.builds_on,
                        t.supervisor_id, b.supervisor_id, r.user_words,
                        t.user_words_cuts, r.user_words_cuts
                 FROM tasks AS t
                 LEFT JOIN tasks AS b ON b.id = t.builds_on
                 LEFT JOIN secondmate_requests AS r ON r.id = t.secondmate_request_id
                 WHERE t.id = ?1",
                params![current],
                |r| {
                    Ok((
                        (
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<qm_types::ids::MessageId>>(1)?,
                            r.get::<_, Option<String>>(7)?,
                        ),
                        r.get::<_, Option<TaskId>>(2)?,
                        r.get::<_, Option<TaskId>>(3)?,
                        r.get::<_, AgentId>(4)?,
                        r.get::<_, Option<AgentId>>(5)?,
                        (
                            r.get::<_, Option<String>>(6)?,
                            r.get::<_, Option<String>>(8)?,
                        ),
                    ))
                },
            )
            .optional()?;
        let Some(((words, message, cuts), retry_of, builds_on, supervisor, builds_sup, request)) =
            row
        else {
            return Ok(UserWords::default());
        };
        if let Some(cuts) = cuts {
            return Ok(UserWords {
                text: words,
                cuts: parse_cuts("tasks.user_words_cuts", &cuts)?,
            });
        }
        if let Some(message) = message
            && let Some(text) = messages::text(conn, message)?
        {
            return Ok(UserWords {
                text,
                cuts: Vec::new(),
            });
        }
        if !words.trim().is_empty() {
            return Ok(UserWords {
                text: words,
                cuts: Vec::new(),
            });
        }
        let next = retry_of.or_else(|| builds_on.filter(|_| builds_sup == Some(supervisor)));
        let Some(next) = next else {
            let (text, cuts) = request;
            let text = text.unwrap_or_default();
            let cuts = match cuts {
                Some(cuts) if !text.trim().is_empty() => {
                    parse_cuts("secondmate_requests.user_words_cuts", &cuts)?
                }
                _ => Vec::new(),
            };
            return Ok(UserWords { text, cuts });
        };
        current = next;
    }
    Ok(UserWords::default())
}

fn parse_cuts(column: &'static str, text: &str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str(text).map_err(|source| StoreError::Json { column, source })
}

/// The task `task` builds on or retries, with its latest reports; `None` if there is no such
/// task.
pub fn prior_task(conn: &Connection, task: TaskId) -> Result<Option<PriorTask>, StoreError> {
    let row = conn
        .query_row(
            "SELECT t.kind, t.status, t.branch, t.base_commit, t.head_commit, p.path
             FROM tasks AS t JOIN projects AS p ON p.id = t.project_id
             WHERE t.id = ?1",
            params![task],
            |r| {
                Ok((
                    r.get::<_, TaskKind>(0)?,
                    r.get::<_, TaskStatus>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<CommitSha>>(3)?,
                    r.get::<_, Option<CommitSha>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, status, branch, base_commit, head_commit, project_path)) = row else {
        return Ok(None);
    };
    Ok(Some(PriorTask {
        task,
        kind,
        status,
        branch,
        base_commit,
        head_commit,
        project_path,
        last_clear: last_report(conn, task, true)?,
        last_any: last_report(conn, task, false)?,
    }))
}

fn last_report(
    conn: &Connection,
    task: TaskId,
    clear_only: bool,
) -> Result<Option<StoredReport>, StoreError> {
    let sql = if clear_only {
        "SELECT source, outcome, headings, answer, reproduced, diagnosis, review_verdict,
                head_commit, final_text
         FROM reports WHERE task_id = ?1 AND source = 'report_call' ORDER BY seq DESC LIMIT 1"
    } else {
        "SELECT source, outcome, headings, answer, reproduced, diagnosis, review_verdict,
                head_commit, final_text
         FROM reports WHERE task_id = ?1 ORDER BY seq DESC LIMIT 1"
    };
    let row = conn
        .query_row(sql, params![task], |r| {
            Ok((
                r.get::<_, ReportSource>(0)?,
                r.get::<_, Option<ReportOutcome>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<bool>>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, Option<ReviewVerdict>>(6)?,
                r.get::<_, Option<CommitSha>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })
        .optional()?;
    let Some((source, outcome, headings, answer, reproduced, diagnosis, verdict, head, text)) = row
    else {
        return Ok(None);
    };
    Ok(Some(StoredReport {
        source,
        outcome,
        headings: parse_headings(&headings)?,
        answer,
        reproduced,
        diagnosis,
        review_verdict: verdict,
        head_commit: head,
        final_text: text,
    }))
}

/// `reports.headings`: an object of heading name to text; non-text values are skipped.
fn parse_headings(json: &str) -> Result<BTreeMap<String, String>, StoreError> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|source| StoreError::Json {
            column: "reports.headings",
            source,
        })?;
    Ok(value
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// The crew and task panes.

/// Token use summed over an agent's turns.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenUse {
    pub turns: u32,
    /// Everything the model read: input plus cache reads and writes.
    pub input: u64,
    /// Everything it wrote, thinking included.
    pub output: u64,
    /// The parts of `input`: uncached input, cache reads and cache writes.
    pub fresh_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The thinking part of `output`.
    pub thinking: u64,
    /// The summed cost of the turns whose engine reported one; `None` if none did.
    pub cost_usd: Option<f64>,
    /// The crewmate's conversation was compacted or cleared, which forgets the turns before it,
    /// so the sums above leave those out.
    pub forgotten: bool,
}

/// A crewmate's agent row, as the task pane shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrewAgent {
    pub id: AgentId,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub account: Option<String>,
}

/// The task's latest routing decision, as the task pane shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingView {
    /// The tier used, else the tier the work landed in (while it waits).
    pub tier: Tier,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub account: Option<String>,
    pub why: String,
}

/// One task the crew tree shows.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenTask {
    pub task: TaskId,
    /// The task (`T-`) the crewmate belongs to.
    pub label: TaskLabelId,
    pub kind: TaskKind,
    pub title: String,
    pub status: TaskStatus,
    pub supervisor: AgentId,
    pub project: String,
    /// The branch the task's work goes back to: the typed base branch, else the project's
    /// default.
    pub target: String,
    /// The task's branch, absent for folder tasks.
    pub branch: Option<String>,
    /// The folder the task works in directly, when it has no branch (in place or a plain folder).
    pub folder: Option<String>,
    pub verification: Option<VerificationLevel>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    /// Its crewmate (the newest, if it had more than one).
    pub agent: Option<CrewAgent>,
    pub routing: Option<RoutingView>,
    /// A merge for it waits for your approval.
    pub pending_merge: bool,
    /// Its crewmate is in a turn right now.
    pub in_turn: bool,
    pub tokens: TokenUse,
    /// The latest tool calls, oldest first, as recorded ("Bash cargo test auth").
    pub activity: Vec<String>,
}

/// A secondmate the crew tree shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondmateView {
    pub id: SecondmateId,
    pub name: String,
    /// Its agent row, if it has one.
    pub agent: Option<AgentId>,
    pub working: bool,
}

/// Everything the crew view is built from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CrewData {
    /// The Quartermaster's agent row, if it exists yet.
    pub quartermaster: Option<AgentId>,
    /// Every task still in its life, oldest first; closed tasks and side exits are left out.
    pub tasks: Vec<OpenTask>,
    /// Active secondmates, in the order they were created.
    pub secondmates: Vec<SecondmateView>,
    /// Every held action still waiting for your answer, oldest first.
    pub pending_holds: Vec<PendingHold>,
}

/// A held action waiting for your answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHold {
    /// `held_actions.kind` as stored (`merge`, `decision`, …; the set grows).
    pub kind: String,
    pub task: Option<TaskId>,
}

/// The statuses of tasks being retired or already closed: never shown, in the crew tree or under a
/// project.
pub(crate) const HIDDEN: &str = "('closed', 'failed', 'stopped', 'discarded')";

/// Reads the crew tree's data.
pub fn crew_data(conn: &Connection) -> Result<CrewData, StoreError> {
    let quartermaster: Option<AgentId> = conn
        .query_row(
            "SELECT id FROM agents WHERE role = 'quartermaster' AND retired_at IS NULL",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let secondmates = secondmates(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT t.id, t.kind, t.title, t.status, t.supervisor_id, p.name,
                COALESCE(t.base_branch, p.default_branch), t.branch, t.verification_level,
                t.created_at, t.started_at,
                EXISTS (SELECT 1 FROM held_actions AS h
                        WHERE h.task_id = t.id AND h.kind = 'merge' AND h.status = 'pending'),
                EXISTS (SELECT 1 FROM turns AS u JOIN agents AS a ON a.id = u.agent_id
                        WHERE a.task_id = t.id AND a.role = 'crewmate' AND u.status = 'running'),
                p.path, COALESCE(t.label_id, t.id)
         FROM tasks AS t JOIN projects AS p ON p.id = t.project_id
         WHERE t.status NOT IN {HIDDEN}
         ORDER BY t.created_at, t.seq"
    ))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(TaskBase {
                task: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                status: r.get(3)?,
                supervisor: r.get(4)?,
                project: r.get(5)?,
                target: r.get(6)?,
                branch: r.get(7)?,
                verification: r.get(8)?,
                created_at: r.get(9)?,
                started_at: r.get(10)?,
                pending_merge: r.get(11)?,
                in_turn: r.get(12)?,
                path: r.get(13)?,
                label: r.get(14)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut tasks = Vec::with_capacity(rows.len());
    for base in rows {
        let task = base.task;
        let agent = crew_agent(conn, task)?;
        let tokens = match &agent {
            Some(_) => token_use(conn, task)?,
            None => TokenUse::default(),
        };
        tasks.push(OpenTask {
            task,
            label: base.label,
            kind: base.kind,
            title: base.title,
            status: base.status,
            supervisor: base.supervisor,
            project: base.project,
            target: base.target,
            folder: base.branch.is_none().then_some(base.path),
            branch: base.branch,
            verification: base.verification,
            created_at: from_millis("tasks.created_at", base.created_at)?,
            started_at: base
                .started_at
                .map(|ms| from_millis("tasks.started_at", ms))
                .transpose()?,
            agent,
            routing: routing(conn, task)?,
            pending_merge: base.pending_merge,
            in_turn: base.in_turn,
            tokens,
            activity: activity(conn, task)?,
        });
    }
    Ok(CrewData {
        quartermaster,
        tasks,
        secondmates,
        pending_holds: pending_holds(conn)?,
    })
}

/// A task row before its agent, routing, tokens and activity are added.
struct TaskBase {
    task: TaskId,
    kind: TaskKind,
    title: String,
    status: TaskStatus,
    supervisor: AgentId,
    project: String,
    target: String,
    branch: Option<String>,
    verification: Option<VerificationLevel>,
    created_at: i64,
    started_at: Option<i64>,
    pending_merge: bool,
    in_turn: bool,
    path: String,
    label: TaskLabelId,
}

fn pending_holds(conn: &Connection) -> Result<Vec<PendingHold>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT kind, task_id FROM held_actions WHERE status = 'pending' ORDER BY seq")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PendingHold {
                kind: r.get(0)?,
                task: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn secondmates(conn: &Connection) -> Result<Vec<SecondmateView>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.name,
                (SELECT a.id FROM agents AS a
                 WHERE a.secondmate_id = s.id AND a.retired_at IS NULL
                 ORDER BY a.seq DESC LIMIT 1)
         FROM secondmates AS s
         WHERE s.retired_at IS NULL
         ORDER BY s.created_at, s.seq",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, SecondmateId>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<AgentId>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, name, agent) in rows {
        let working = match agent {
            Some(agent) => agent_working(conn, agent)?,
            None => false,
        };
        out.push(SecondmateView {
            id,
            name,
            agent,
            working,
        });
    }
    Ok(out)
}

/// An agent is working while its row says `running` or it has a turn running.
fn agent_working(conn: &Connection, agent: AgentId) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT a.status = 'running'
                OR EXISTS (SELECT 1 FROM turns AS t WHERE t.agent_id = a.id AND t.status = 'running')
         FROM agents AS a WHERE a.id = ?1",
        params![agent],
        |r| r.get(0),
    )?)
}

fn crew_agent(conn: &Connection, task: TaskId) -> Result<Option<CrewAgent>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT a.id, a.status, a.model, a.effort, c.label
             FROM agents AS a LEFT JOIN accounts AS c ON c.id = a.account_id
             WHERE a.task_id = ?1 AND a.role = 'crewmate'
             ORDER BY a.seq DESC LIMIT 1",
            params![task],
            |r| {
                Ok(CrewAgent {
                    id: r.get(0)?,
                    status: r.get(1)?,
                    model: r.get(2)?,
                    effort: r.get(3)?,
                    account: r.get(4)?,
                })
            },
        )
        .optional()?)
}

fn routing(conn: &Connection, task: TaskId) -> Result<Option<RoutingView>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(d.tier_used, d.tier_landed), d.model, d.effort, c.label, d.why
             FROM routing_decisions AS d LEFT JOIN accounts AS c ON c.id = d.account_id
             WHERE d.task_id = ?1
             ORDER BY d.seq DESC LIMIT 1",
            params![task],
            |r| {
                Ok(RoutingView {
                    tier: r.get(0)?,
                    model: r.get(1)?,
                    effort: r.get(2)?,
                    account: r.get(3)?,
                    why: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Token use summed over every turn of every crewmate the task has had.
pub fn token_use(conn: &Connection, task: TaskId) -> Result<TokenUse, StoreError> {
    type Sums = (i64, i64, i64, i64, f64, i64, i64, i64, i64);
    let (turns, input, output, costed, cost, fresh, read, write, thinking): Sums = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(t.input_tokens + t.cache_read_tokens + t.cache_write_tokens), 0),
                COALESCE(SUM(t.output_tokens), 0),
                COUNT(t.cost_usd),
                COALESCE(SUM(t.cost_usd), 0.0),
                COALESCE(SUM(t.input_tokens), 0),
                COALESCE(SUM(t.cache_read_tokens), 0),
                COALESCE(SUM(t.cache_write_tokens), 0),
                COALESCE(SUM(t.thinking_tokens), 0)
         FROM turns AS t JOIN agents AS a ON a.id = t.agent_id
         WHERE a.task_id = ?1 AND a.role = 'crewmate'",
        params![task],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        },
    )?;
    let forgotten: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events AS e JOIN agents AS a ON a.id = e.agent_id
                        WHERE a.task_id = ?1 AND a.role = 'crewmate' AND e.kind IN (?2, ?3))",
        params![
            task,
            crate::turns::kind::COMPACTED,
            crate::turns::kind::CLEARED
        ],
        |r| r.get(0),
    )?;
    let count = |column: &'static str, n: i64| {
        u64::try_from(n).map_err(|_| StoreError::Invalid {
            column,
            message: format!("{n} is negative"),
        })
    };
    Ok(TokenUse {
        turns: u32::try_from(turns).unwrap_or(u32::MAX),
        input: count("turns.input_tokens", input)?,
        output: count("turns.output_tokens", output)?,
        fresh_input: count("turns.input_tokens", fresh)?,
        cache_read: count("turns.cache_read_tokens", read)?,
        cache_write: count("turns.cache_write_tokens", write)?,
        thinking: count("turns.thinking_tokens", thinking)?,
        cost_usd: (costed > 0).then_some(cost),
        forgotten,
    })
}

/// The task's latest tool calls, oldest first.
fn activity(conn: &Connection, task: TaskId) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT summary FROM events WHERE task_id = ?1 AND kind = ?2 ORDER BY seq DESC LIMIT ?3",
    )?;
    let limit = i64::try_from(ACTIVITY_ROWS).unwrap_or(i64::MAX);
    let mut rows = stmt
        .query_map(params![task, TASK_TOOL_EVENT, limit], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.reverse();
    Ok(rows)
}
