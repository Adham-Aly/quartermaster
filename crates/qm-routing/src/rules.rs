//! The rule table: declared facts to a tier.
//!
//! "A fixed rule table maps the facts to a tier. It reads only the declared facts, never the
//! task's text." Each task kind is one row, and two more rows cover work that builds on finished
//! thinking: a build on a finished plan, and a fix on a confirmed diagnosis. You set each row's
//! tier in Settings ▸ Routing ▸ Task kinds (`config.toml`'s `[routing.kinds]`, read as
//! [`KindTiers`]); a row you haven't set follows its default ([`KindRow::default_tier`]): lookups
//! run on low, work building on finished thinking on medium, and everything else on high. The
//! table is built from the live config each time a task routes ([`RuleTable::from_kinds`]), so a
//! change applies to the next task routed.
//!
//! The `builds_on` fact checks live here too ([`builds_on_fact`]): "'Finished plan' and
//! 'confirmed diagnosis' are facts the app checks, not words the Quartermaster writes. `builds_on`
//! names a task, and the app checks it's a `plan` task that reported an answer, or a reproduction
//! that reported `reproduced: yes` (or a `research` task whose report is marked as a diagnosis).
//! Anything else is treated as building on nothing."

use crate::record::BuildsOnFact;
use qm_types::config::KindTiers;
use qm_types::domain::{KindRow, TaskKind, Tier};
use std::fmt;

/// The row that work of `kind` building on `fact` routes by: a build on a finished plan and a fix
/// on a confirmed diagnosis have their own rows; everything else routes by its kind's row.
pub const fn row_for(kind: TaskKind, fact: BuildsOnFact) -> KindRow {
    match (kind, fact) {
        (TaskKind::Build, BuildsOnFact::FinishedPlan) => KindRow::BuildOnFinishedPlan,
        (TaskKind::Fix, BuildsOnFact::ConfirmedDiagnosis) => KindRow::FixOnConfirmedDiagnosis,
        _ => KindRow::for_kind(kind),
    }
}

/// The rule table: every row's tier, yours or the default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleTable {
    kinds: KindTiers,
}

/// The tier the table gave, and the row that gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Matched {
    pub tier: Tier,
    pub row: KindRow,
    /// You set this row to a tier other than its default.
    pub yours: bool,
}

impl fmt::Display for Matched {
    /// The rule as the routing record stores it, e.g. `kind = "fix", builds_on =
    /// "confirmed_diagnosis" → medium (default)` or `kind = "review" → premium (your setting)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kind = \"{}\"", self.row.kind())?;
        match self.row {
            KindRow::BuildOnFinishedPlan => write!(f, ", builds_on = \"finished_plan\"")?,
            KindRow::FixOnConfirmedDiagnosis => {
                write!(f, ", builds_on = \"confirmed_diagnosis\"")?;
            }
            _ => {}
        }
        let from = if self.yours {
            "your setting"
        } else {
            "default"
        };
        write!(f, " → {} ({from})", self.tier)
    }
}

impl RuleTable {
    /// The table for these settings.
    pub fn from_kinds(kinds: &KindTiers) -> Self {
        Self {
            kinds: kinds.clone(),
        }
    }

    /// The tier for work of `kind` building on `fact`: its row's.
    pub fn tier_for(&self, kind: TaskKind, fact: BuildsOnFact) -> Matched {
        let row = row_for(kind, fact);
        Matched {
            tier: self.kinds.tier(row),
            row,
            yours: self.kinds.changed(row),
        }
    }
}

impl BuildsOnFact {
    /// Every fact, in declaration order.
    pub const ALL: [Self; 3] = [Self::FinishedPlan, Self::ConfirmedDiagnosis, Self::Nothing];

    /// The wire name, as the routing record writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FinishedPlan => "finished_plan",
            Self::ConfirmedDiagnosis => "confirmed_diagnosis",
            Self::Nothing => "nothing",
        }
    }
}

/// What the app knows about the task named in `builds_on`, read from its row and its report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildsOnTarget {
    /// The named task's kind; `None` if no such task exists.
    pub kind: Option<TaskKind>,
    /// The task reported an answer (answer-only kinds).
    pub reported_answer: bool,
    /// A reproduction's `reproduced: yes|no`, if it reported one.
    pub reproduced: Option<bool>,
    /// A research report marked as a diagnosis.
    pub marked_diagnosis: bool,
}

/// The `builds_on` fact check: "a `plan` task that reported an answer" is a finished plan; "a
/// reproduction that reported `reproduced: yes` (or a `research` task whose report is marked as a
/// diagnosis)" is a confirmed diagnosis; "anything else is treated as building on nothing".
///
/// A marked research report must also have reported an answer, since the mark is on the report.
pub fn builds_on_fact(target: &BuildsOnTarget) -> BuildsOnFact {
    match target.kind {
        Some(TaskKind::Plan) if target.reported_answer => BuildsOnFact::FinishedPlan,
        Some(TaskKind::Reproduce) if target.reproduced == Some(true) => {
            BuildsOnFact::ConfirmedDiagnosis
        }
        Some(TaskKind::Research) if target.reported_answer && target.marked_diagnosis => {
            BuildsOnFact::ConfirmedDiagnosis
        }
        _ => BuildsOnFact::Nothing,
    }
}
