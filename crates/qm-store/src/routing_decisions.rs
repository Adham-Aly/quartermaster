//! Routing decisions.
//!
//! One row per decision: the declared facts, the rule that matched, who decided the kind, the tier
//! the work landed in, the tier, model, account and effort actually used, every model skipped and
//! why, every fallback step, and the one plain-language "why" line. A row with no model is
//! waiting. The JSON columns hold `qm_routing::record` types; this crate stores them as JSON
//! without depending on `qm-routing` (the core serialises them).

use crate::StoreError;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{Effort, Tier};
use qm_types::ids::{AccountId, AgentId, ModelId, RoutingDecisionId, TaskId};
use rusqlite::{Connection, params};

/// The pair and effort a decision chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionChoice {
    pub tier: Tier,
    /// The canonical model id.
    pub model: ModelId,
    pub account: AccountId,
    pub effort: Effort,
}

/// One decision, as written and read.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutingDecision {
    pub task: TaskId,
    pub agent: Option<AgentId>,
    /// `DeclaredFacts` as JSON.
    pub facts: serde_json::Value,
    pub rule: String,
    /// `declared` or `model:<id>`.
    pub decided_by: String,
    pub tier_landed: Tier,
    /// `None` while waiting.
    pub choice: Option<DecisionChoice>,
    /// `Vec<SkippedModel>` as JSON.
    pub skipped: serde_json::Value,
    /// `Vec<FallbackStep>` as JSON.
    pub fallbacks: serde_json::Value,
    pub why: String,
    pub at: Timestamp,
}

fn json(column: &'static str, v: &serde_json::Value) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json { column, source })
}

fn parse(column: &'static str, s: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(s).map_err(|source| StoreError::Json { column, source })
}

/// Records a decision; called in the transaction that starts the crewmate.
pub fn insert_decision(
    conn: &Connection,
    d: &RoutingDecision,
) -> Result<RoutingDecisionId, StoreError> {
    let c = d.choice.as_ref();
    let id = conn.query_row(
        "INSERT INTO routing_decisions (task_id, agent_id, facts, rule, decided_by, tier_landed,
             tier_used, model, account_id, effort, skipped, fallbacks, why, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         RETURNING id",
        params![
            d.task,
            d.agent,
            json("routing_decisions.facts", &d.facts)?,
            d.rule,
            d.decided_by,
            d.tier_landed,
            c.map(|c| c.tier),
            c.map(|c| &c.model),
            c.map(|c| c.account),
            c.map(|c| c.effort),
            json("routing_decisions.skipped", &d.skipped)?,
            json("routing_decisions.fallbacks", &d.fallbacks)?,
            d.why,
            to_millis(d.at),
            seq::next(d.at),
        ],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Names the agent a decision started, once it exists.
pub fn set_decision_agent(
    conn: &Connection,
    id: RoutingDecisionId,
    agent: AgentId,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE routing_decisions SET agent_id = ?2 WHERE id = ?1",
        params![id, agent],
    )?;
    Ok(())
}

/// A task's decisions, oldest first (a waiting decision is followed by the one that routed it).
pub fn decisions_for_task(
    conn: &Connection,
    task: TaskId,
) -> Result<Vec<(RoutingDecisionId, RoutingDecision)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, facts, rule, decided_by, tier_landed, tier_used, model, account_id,
                effort, skipped, fallbacks, why, created_at
         FROM routing_decisions WHERE task_id = ?1 ORDER BY seq",
    )?;
    let raw = stmt
        .query_map(params![task], |r| {
            Ok((
                r.get::<_, RoutingDecisionId>(0)?,
                r.get::<_, Option<AgentId>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Tier>(5)?,
                (
                    r.get::<_, Option<Tier>>(6)?,
                    r.get::<_, Option<ModelId>>(7)?,
                    r.get::<_, Option<AccountId>>(8)?,
                    r.get::<_, Option<Effort>>(9)?,
                ),
                r.get::<_, String>(10)?,
                r.get::<_, String>(11)?,
                r.get::<_, String>(12)?,
                r.get::<_, i64>(13)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(
                id,
                agent,
                facts,
                rule,
                decided_by,
                tier_landed,
                used,
                skipped,
                fallbacks,
                why,
                at,
            )| {
                let choice = match used {
                    (Some(tier), Some(model), Some(account), Some(effort)) => {
                        Some(DecisionChoice {
                            tier,
                            model,
                            account,
                            effort,
                        })
                    }
                    _ => None,
                };
                Ok((
                    id,
                    RoutingDecision {
                        task,
                        agent,
                        facts: parse("routing_decisions.facts", &facts)?,
                        rule,
                        decided_by,
                        tier_landed,
                        choice,
                        skipped: parse("routing_decisions.skipped", &skipped)?,
                        fallbacks: parse("routing_decisions.fallbacks", &fallbacks)?,
                        why,
                        at: from_millis("routing_decisions.created_at", at)?,
                    },
                ))
            },
        )
        .collect()
}
