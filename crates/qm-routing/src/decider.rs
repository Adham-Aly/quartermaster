//! Room for a decision model.
//!
//! Two judgments go through one [`Decider`]: the kind of a task, and whether a request warrants a
//! secondmate. The MVP's only implementation, [`DeclaredDecider`], reads the Quartermaster's typed
//! field. Later, a one-shot decision model (one tiny call, one word from a fixed list, no tools, no
//! session) can replace it without touching anything else.
//!
//! The caller's rules, which stay the same whatever implementation is plugged in: skip the
//! decider when the outcome is already decided (you named a model, asked for premium, or it is a
//! stronger-model retry); ask it at most once per task; and if it fails, times out or can't decide,
//! route high ([`Decided::kind_or_unclear`]). It never blocks a task.

use qm_types::BoxFuture;
use qm_types::domain::TaskKind;
use qm_types::error::IdError;
use qm_types::ids::ModelId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// The facts the kind question is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindQuestion<'a> {
    /// The kind the Quartermaster declared in `start_crewmate`.
    pub declared: TaskKind,
    pub title: &'a str,
    pub goal: &'a str,
    /// Your words, copied from your message.
    pub user_words: &'a str,
}

/// The facts the secondmate question is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecondmateQuestion<'a> {
    /// The Quartermaster's own judgment, from its typed field.
    pub declared: bool,
    pub request: &'a str,
    pub user_words: &'a str,
}

/// A decider's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decided<T> {
    Answer {
        value: T,
        by: DecidedBy,
    },
    /// It failed, timed out or couldn't decide.
    Undecided {
        reason: String,
    },
}

impl<T> Decided<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Answer { value, .. } => Some(value),
            Self::Undecided { .. } => None,
        }
    }
}

impl Decided<TaskKind> {
    /// The decided kind, or `unclear` (routed by the unclear row) when undecided.
    pub fn kind_or_unclear(&self) -> TaskKind {
        self.value().copied().unwrap_or(TaskKind::Unclear)
    }
}

/// Who answered, recorded in the routing record (`routing_decisions.decided_by`): `declared`, or
/// `model:<id>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DecidedBy {
    /// The Quartermaster's typed field.
    Declared,
    /// A decision model.
    Model { model: ModelId },
}

impl fmt::Display for DecidedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declared => f.write_str("declared"),
            Self::Model { model } => write!(f, "model:{model}"),
        }
    }
}

impl FromStr for DecidedBy {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        if s == "declared" {
            return Ok(Self::Declared);
        }
        match s.strip_prefix("model:") {
            Some(model) => Ok(Self::Model {
                model: model.parse()?,
            }),
            None => Err(IdError {
                kind: "decider",
                input: s.to_owned(),
                why: "expected declared or model:<id>",
            }),
        }
    }
}

impl Serialize for DecidedBy {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DecidedBy {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The one interface for both judgments.
pub trait Decider: Send + Sync + fmt::Debug {
    fn task_kind<'a>(&'a self, question: KindQuestion<'a>) -> BoxFuture<'a, Decided<TaskKind>>;

    fn warrants_secondmate<'a>(
        &'a self,
        question: SecondmateQuestion<'a>,
    ) -> BoxFuture<'a, Decided<bool>>;
}

/// The MVP decider: the Quartermaster's typed field is the answer.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeclaredDecider;

impl Decider for DeclaredDecider {
    fn task_kind<'a>(&'a self, question: KindQuestion<'a>) -> BoxFuture<'a, Decided<TaskKind>> {
        Box::pin(std::future::ready(Decided::Answer {
            value: question.declared,
            by: DecidedBy::Declared,
        }))
    }

    fn warrants_secondmate<'a>(
        &'a self,
        question: SecondmateQuestion<'a>,
    ) -> BoxFuture<'a, Decided<bool>> {
        Box::pin(std::future::ready(Decided::Answer {
            value: question.declared,
            by: DecidedBy::Declared,
        }))
    }
}
