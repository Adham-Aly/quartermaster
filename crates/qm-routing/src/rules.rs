//! The rule table: declared facts to a tier.
//!
//! "A fixed rule table maps the facts to a tier: low, medium or high. It reads only the declared
//! facts, never the task's text. 'Unclear' always routes high, so a wrong or uncertain label costs
//! more than needed but never runs weaker. The table is data, not scattered logic." It "ships as a
//! data file inside the binary, which the rule engine loads and validates at startup. A table that
//! maps anything to premium is rejected."
//!
//! The file is `crates/qm-routing/routing.toml`, embedded with `include_str!` as
//! [`ROUTING_TOML`]. The binary calls [`RuleTable::embedded`] at startup and refuses to start on an
//! error.
//!
//! The `builds_on` fact checks live here too ([`builds_on_fact`]): "'Finished plan' and
//! 'confirmed diagnosis' are facts the app checks, not words the Quartermaster writes. `builds_on`
//! names a task, and the app checks it's a `plan` task that reported an answer, or a task of any
//! kind that reported `reproduced: yes` or marked its report as a diagnosis. Anything else is
//! treated as building on nothing."

use crate::record::BuildsOnFact;
use qm_types::domain::{TaskKind, Tier};
use serde::Deserialize;
use std::fmt;

/// The rule table as shipped, embedded in the binary.
pub const ROUTING_TOML: &str = include_str!("../routing.toml");

/// Why a rule table was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuleTableError {
    #[error("the routing table can't be read: {0}")]
    Parse(String),
    #[error("the routing table has no rules")]
    Empty,
    /// "A table that maps anything to premium is rejected": premium is never automatic.
    #[error("rule {index} ({rule}) maps to premium; premium is never picked automatically")]
    Premium { index: usize, rule: String },
    /// `builds_on = "nothing"` is what every unchecked fact becomes, so it is not a rule input.
    #[error("rule {index} ({rule}) names builds_on = \"nothing\"; name a checked fact or none")]
    BuildsOnNothing { index: usize, rule: String },
    /// "'Unclear' always routes high".
    #[error("unclear work must always route high, but {facts} routes {tier}")]
    UnclearNotHigh { facts: String, tier: Tier },
    /// Every combination of facts must land somewhere.
    #[error("{facts} matches no rule; end the table with a kind = \"*\" rule")]
    Unmatched { facts: String },
}

/// A rule's kind pattern: one kind, or `*` for every kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindPattern {
    Any,
    Kind(TaskKind),
}

impl KindPattern {
    pub fn matches(self, kind: TaskKind) -> bool {
        match self {
            Self::Any => true,
            Self::Kind(k) => k == kind,
        }
    }
}

impl fmt::Display for KindPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Kind(k) => f.write_str(k.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for KindPattern {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "*" {
            Ok(Self::Any)
        } else {
            s.parse().map(Self::Kind).map_err(serde::de::Error::custom)
        }
    }
}

/// One row of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub kind: KindPattern,
    /// When set, the rule matches only work that builds on this checked fact.
    #[serde(default)]
    pub builds_on: Option<BuildsOnFact>,
    pub tier: Tier,
}

impl Rule {
    pub fn matches(&self, kind: TaskKind, fact: BuildsOnFact) -> bool {
        self.kind.matches(kind) && self.builds_on.is_none_or(|b| b == fact)
    }
}

impl fmt::Display for Rule {
    /// The rule as the routing record stores it, e.g. `kind = "fix", builds_on =
    /// "confirmed_diagnosis" → medium`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kind = \"{}\"", self.kind)?;
        if let Some(b) = self.builds_on {
            write!(f, ", builds_on = \"{}\"", b.as_str())?;
        }
        write!(f, " → {}", self.tier)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableFile {
    rules: Vec<Rule>,
}

/// A validated rule table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleTable {
    rules: Vec<Rule>,
}

/// The tier the table gave, and the rule that matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Matched {
    pub tier: Tier,
    pub rule: Rule,
    /// The rule's position in the table, from 0.
    pub index: usize,
}

impl RuleTable {
    /// The table shipped inside the binary, loaded and validated (call at startup).
    pub fn embedded() -> Result<Self, RuleTableError> {
        Self::parse(ROUTING_TOML)
    }

    /// Parses and validates a table.
    pub fn parse(text: &str) -> Result<Self, RuleTableError> {
        let file: TableFile =
            toml::from_str(text).map_err(|e| RuleTableError::Parse(e.message().to_owned()))?;
        Self::new(file.rules)
    }

    /// Validates rules into a table: not empty, no premium, no `builds_on = "nothing"`, every
    /// combination of facts matched, and `unclear` routing high whatever it builds on.
    pub fn new(rules: Vec<Rule>) -> Result<Self, RuleTableError> {
        if rules.is_empty() {
            return Err(RuleTableError::Empty);
        }
        for (index, rule) in rules.iter().enumerate() {
            if rule.tier == Tier::Premium {
                return Err(RuleTableError::Premium {
                    index,
                    rule: rule.to_string(),
                });
            }
            if rule.builds_on == Some(BuildsOnFact::Nothing) {
                return Err(RuleTableError::BuildsOnNothing {
                    index,
                    rule: rule.to_string(),
                });
            }
        }
        let table = Self { rules };
        for &kind in TaskKind::ALL {
            for fact in BuildsOnFact::ALL {
                let facts = format!("kind = \"{kind}\", builds on {}", fact.as_str());
                let Some(m) = table.lookup(kind, fact) else {
                    return Err(RuleTableError::Unmatched { facts });
                };
                if kind == TaskKind::Unclear && m.tier != Tier::High {
                    return Err(RuleTableError::UnclearNotHigh {
                        facts,
                        tier: m.tier,
                    });
                }
            }
        }
        Ok(table)
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    fn lookup(&self, kind: TaskKind, fact: BuildsOnFact) -> Option<Matched> {
        self.rules
            .iter()
            .enumerate()
            .find(|(_, r)| r.matches(kind, fact))
            .map(|(index, rule)| Matched {
                tier: rule.tier,
                rule: *rule,
                index,
            })
    }

    /// The first matching rule's tier: "the first matching rule wins". Never premium, and always
    /// some rule (validated in [`RuleTable::new`]).
    pub fn tier_for(&self, kind: TaskKind, fact: BuildsOnFact) -> Matched {
        self.lookup(kind, fact).unwrap_or(Matched {
            // Unreachable for a validated table; high is the safe answer ("never runs weaker").
            tier: Tier::High,
            rule: Rule {
                kind: KindPattern::Any,
                builds_on: None,
                tier: Tier::High,
            },
            index: self.rules.len(),
        })
    }
}

impl BuildsOnFact {
    /// Every fact, in declaration order.
    pub const ALL: [Self; 3] = [Self::FinishedPlan, Self::ConfirmedDiagnosis, Self::Nothing];

    /// The wire name, as written in `routing.toml`.
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
    /// The task reported an answer.
    pub reported_answer: bool,
    /// A reproduction's `reproduced: yes|no`, if it reported one.
    pub reproduced: Option<bool>,
    /// A report marked as a diagnosis (any kind may mark one).
    pub marked_diagnosis: bool,
}

/// The `builds_on` fact check: "a `plan` task that reported an answer" is a finished plan; any task
/// that reported `reproduced: yes`, or whose report is marked as a diagnosis, is a confirmed
/// diagnosis, whatever its kind; "anything else is treated as building on nothing".
pub fn builds_on_fact(target: &BuildsOnTarget) -> BuildsOnFact {
    match target.kind {
        Some(TaskKind::Plan) if target.reported_answer => BuildsOnFact::FinishedPlan,
        Some(_) if target.reproduced == Some(true) || target.marked_diagnosis => {
            BuildsOnFact::ConfirmedDiagnosis
        }
        None | Some(_) => BuildsOnFact::Nothing,
    }
}
