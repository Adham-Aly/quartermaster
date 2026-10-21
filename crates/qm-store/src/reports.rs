//! Crewmates' reports.
//!
//! A `report` call is a clear report (`source = report_call`, with an outcome); a turn that ended
//! without one keeps its final text as a `final_text` report, which the supervisor can read but
//! which wakes nobody.

use crate::StoreError;
use crate::ids;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{ReportOutcome, ReportSource, ReviewVerdict, RiskLevel};
use qm_types::ids::space;
use qm_types::ids::{AgentId, CommitSha, ReportId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};

/// A report to write.
#[derive(Clone, Debug, PartialEq)]
pub struct NewReport<'a> {
    pub task: TaskId,
    pub agent: AgentId,
    pub source: ReportSource,
    pub outcome: Option<ReportOutcome>,
    /// The five headings, as a JSON object.
    pub headings: serde_json::Value,
    pub answer: Option<&'a str>,
    pub reproduced: Option<bool>,
    pub diagnosis: bool,
    pub review_verdict: Option<ReviewVerdict>,
    /// `Vec<Finding>` as JSON.
    pub review_findings: serde_json::Value,
    pub review_risk: Option<RiskLevel>,
    pub review_risk_reason: Option<&'a str>,
    /// `Vec<String>` as JSON.
    pub blockers: serde_json::Value,
    pub head_commit: Option<&'a CommitSha>,
    pub final_text: Option<&'a str>,
    pub at: Timestamp,
}

impl NewReport<'_> {
    /// A report with nothing in it but who, what and when; set the rest after.
    pub fn empty(task: TaskId, agent: AgentId, at: Timestamp) -> Self {
        Self {
            task,
            agent,
            source: ReportSource::ReportCall,
            outcome: None,
            headings: serde_json::json!({}),
            answer: None,
            reproduced: None,
            diagnosis: false,
            review_verdict: None,
            review_findings: serde_json::json!([]),
            review_risk: None,
            review_risk_reason: None,
            blockers: serde_json::json!([]),
            head_commit: None,
            final_text: None,
            at,
        }
    }
}

/// A stored report.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportRow {
    pub id: ReportId,
    /// Creation order across every table with a `seq` (see `seq.rs`).
    pub seq: i64,
    pub task: TaskId,
    pub agent: AgentId,
    pub source: ReportSource,
    pub outcome: Option<ReportOutcome>,
    pub headings: serde_json::Value,
    pub answer: Option<String>,
    pub reproduced: Option<bool>,
    pub diagnosis: bool,
    pub review_verdict: Option<ReviewVerdict>,
    pub review_findings: serde_json::Value,
    pub review_risk: Option<RiskLevel>,
    pub review_risk_reason: Option<String>,
    pub blockers: serde_json::Value,
    pub head_commit: Option<String>,
    pub final_text: Option<String>,
    pub at: Timestamp,
}

fn json(column: &'static str, v: &serde_json::Value) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json { column, source })
}

fn parse(column: &'static str, s: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(s).map_err(|source| StoreError::Json { column, source })
}

/// Writes one report; call inside the transaction that records its consequences.
pub fn insert(conn: &Connection, r: &NewReport<'_>) -> Result<ReportId, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO reports (task_id, agent_id, source, outcome, headings, answer, reproduced,
             diagnosis, review_verdict, review_findings, review_risk, review_risk_reason, blockers,
             head_commit, final_text, created_at, seq, id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
         RETURNING id",
        params![
            r.task,
            r.agent,
            r.source,
            r.outcome,
            json("reports.headings", &r.headings)?,
            r.answer,
            r.reproduced.map(i64::from),
            i64::from(r.diagnosis),
            r.review_verdict,
            json("reports.review_findings", &r.review_findings)?,
            r.review_risk,
            r.review_risk_reason,
            json("reports.blockers", &r.blockers)?,
            r.head_commit.map(CommitSha::as_str),
            r.final_text,
            to_millis(r.at),
            seq::next(r.at),
            ids::fresh(conn, "reports", space::ROW)?,
        ],
        |row| row.get(0),
    )?)
}

const COLUMNS: &str = "id, task_id, agent_id, source, outcome, headings, answer, reproduced,
    diagnosis, review_verdict, review_findings, review_risk, review_risk_reason, blockers,
    head_commit, final_text, created_at, seq";

type Raw = (
    (
        ReportId,
        TaskId,
        AgentId,
        ReportSource,
        Option<ReportOutcome>,
    ),
    (String, Option<String>, Option<bool>, bool),
    (
        Option<ReviewVerdict>,
        String,
        Option<RiskLevel>,
        Option<String>,
    ),
    (String, Option<String>, Option<String>, i64, i64),
);

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
        (r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?),
        (r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?),
        (r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?, r.get(17)?),
    ))
}

fn build(raw: Raw) -> Result<ReportRow, StoreError> {
    let (
        (id, task, agent, source, outcome),
        (headings, answer, reproduced, diagnosis),
        (review_verdict, findings, review_risk, review_risk_reason),
        (blockers, head_commit, final_text, at, seq),
    ) = raw;
    Ok(ReportRow {
        id,
        task,
        agent,
        source,
        outcome,
        headings: parse("reports.headings", &headings)?,
        answer,
        reproduced,
        diagnosis,
        review_verdict,
        review_findings: parse("reports.review_findings", &findings)?,
        review_risk,
        review_risk_reason,
        blockers: parse("reports.blockers", &blockers)?,
        head_commit,
        final_text,
        at: from_millis("reports.created_at", at)?,
        seq,
    })
}

/// The task's newest clear report (a `report` call), if any.
pub fn latest_clear(conn: &Connection, task: TaskId) -> Result<Option<ReportRow>, StoreError> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM reports WHERE task_id = ?1 AND source = 'report_call'
                 ORDER BY seq DESC LIMIT 1"
            ),
            params![task],
            raw,
        )
        .optional()?;
    raw.map(build).transpose()
}

/// Every report of the task, oldest first.
pub fn for_task(conn: &Connection, task: TaskId) -> Result<Vec<ReportRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM reports WHERE task_id = ?1 ORDER BY seq"
    ))?;
    stmt.query_map(params![task], raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(build)
        .collect()
}
