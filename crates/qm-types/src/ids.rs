//! IDs.
//!
//! **Display IDs** are what agents and you see and type: `T-0042` (task), `SM-02` (secondmate),
//! `C-0193` (correlation ID of a request routed to a secondmate), `J3` (background job), `SI-5`
//! (standing instruction) and `SC-7` (schedule). Each wraps the positive integer that is its
//! database key; it serialises as its display string (so tool arguments, tool results and events
//! read naturally) and is stored as an `INTEGER`. Parsing is strict about the prefix and lenient
//! about zero padding: `T-42` parses and displays as `T-0042`.
//!
//! A task's git names derive from its ID: branch `qm/task-0042` and kept ref
//! `refs/quartermaster/kept/T-0042`, where unmerged work is saved before a worktree is retired.
//!
//! **Row IDs** ([`AgentId`], [`TurnId`], [`WakeId`], …) are internal database keys with no display
//! form; they serialise as plain numbers.

use crate::error::IdError;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

/// Parses the digits after a display ID's prefix: ASCII digits only, at least one, non-zero,
/// within `u32`.
fn parse_number(kind: &'static str, input: &str, digits: &str) -> Result<NonZeroU32, IdError> {
    if digits.is_empty() {
        return Err(IdError::new(kind, input, "the number is missing"));
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(IdError::new(
            kind,
            input,
            "only digits may follow the prefix",
        ));
    }
    let n: u32 = digits
        .parse()
        .map_err(|_| IdError::new(kind, input, "the number is too large"))?;
    NonZeroU32::new(n).ok_or_else(|| IdError::new(kind, input, "IDs start at 1"))
}

macro_rules! display_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $prefix:literal, $width:literal, $pattern:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroU32);

        impl $name {
            /// The fixed prefix of the display form.
            pub const PREFIX: &'static str = $prefix;

            /// The ID with this number; `None` for 0, since IDs start at 1.
            pub const fn new(n: u32) -> Option<Self> {
                match NonZeroU32::new(n) {
                    Some(n) => Some(Self(n)),
                    None => None,
                }
            }

            /// The number, which is also the database key.
            pub const fn get(self) -> u32 {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{:0width$}", Self::PREFIX, self.0, width = $width)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                let digits = s
                    .strip_prefix(Self::PREFIX)
                    .ok_or_else(|| IdError::new($kind, s, concat!("it must start with ", $prefix)))?;
                parse_number($kind, s, digits).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = Cow::<'de, str>::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn inline_schema() -> bool {
                true
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string", "pattern": $pattern })
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(i64::from(self.get())))
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                let n = v.as_i64()?;
                u32::try_from(n)
                    .ok()
                    .and_then(Self::new)
                    .ok_or(rusqlite::types::FromSqlError::OutOfRange(n))
            }
        }
    };
}

display_id!(
    /// A task, shown as `T-0042`.
    TaskId, "task ID", "T-", 4, "^T-[0-9]+$"
);
display_id!(
    /// A secondmate, shown as `SM-02`.
    SecondmateId, "secondmate ID", "SM-", 2, "^SM-[0-9]+$"
);
display_id!(
    /// A request routed to a secondmate, shown as `C-0193`; its answer on the parent channel
    /// carries the same ID.
    CorrelationId, "correlation ID", "C-", 4, "^C-[0-9]+$"
);
display_id!(
    /// A crewmate's background shell job, shown as `J3`.
    JobId, "job ID", "J", 0, "^J[0-9]+$"
);
display_id!(
    /// One of your standing instructions, shown as `SI-5`.
    StandingInstructionId, "standing instruction ID", "SI-", 0, "^SI-[0-9]+$"
);
display_id!(
    /// A scheduled wake, shown as `SC-7`, so `cancel_schedule` can name it.
    ScheduleId, "schedule ID", "SC-", 0, "^SC-[0-9]+$"
);

impl TaskId {
    /// Prefix of every task branch.
    pub const BRANCH_PREFIX: &'static str = "qm/task-";
    /// Prefix of every kept ref.
    pub const KEPT_REF_PREFIX: &'static str = "refs/quartermaster/kept/";

    /// The task's branch, `qm/task-0042`.
    pub fn branch(self) -> String {
        format!("{}{:04}", Self::BRANCH_PREFIX, self.0)
    }

    /// The task a branch belongs to, if it is a task branch.
    pub fn from_branch(branch: &str) -> Result<Self, IdError> {
        let digits = branch
            .strip_prefix(Self::BRANCH_PREFIX)
            .ok_or_else(|| IdError::new("task branch", branch, "it must start with qm/task-"))?;
        parse_number("task branch", branch, digits).map(Self)
    }

    /// The ref unmerged work is saved to before the worktree is retired,
    /// `refs/quartermaster/kept/T-0042`.
    pub fn kept_ref(self) -> String {
        format!("{}{self}", Self::KEPT_REF_PREFIX)
    }

    /// The task a kept ref belongs to.
    pub fn from_kept_ref(git_ref: &str) -> Result<Self, IdError> {
        git_ref
            .strip_prefix(Self::KEPT_REF_PREFIX)
            .ok_or_else(|| {
                IdError::new(
                    "kept ref",
                    git_ref,
                    "it must start with refs/quartermaster/kept/",
                )
            })?
            .parse()
    }
}

macro_rules! row_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            pub const fn new(id: i64) -> Self {
                Self(id)
            }

            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.0))
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                v.as_i64().map(Self)
            }
        }
    };
}

row_id!(
    /// A row in `agents`: the Quartermaster, a secondmate or a crewmate.
    AgentId
);
row_id!(
    /// A row in `turns`.
    TurnId
);
row_id!(
    /// A row in `wakes`.
    WakeId
);
row_id!(
    /// A row in `events`.
    EventId
);
row_id!(
    /// A row in `messages`.
    MessageId
);
row_id!(
    /// A row in `held_actions`.
    HeldActionId
);
row_id!(
    /// A row in `projects`.
    ProjectId
);
row_id!(
    /// A row in `accounts`.
    AccountId
);
row_id!(
    /// A row in `reports`.
    ReportId
);
row_id!(
    /// A row in `routing_decisions`.
    RoutingDecisionId
);
row_id!(
    /// A row in `processes`: one recorded process group.
    ProcessRecordId
);

/// A git commit, as a full lowercase hex SHA (40 characters, or 64 in a SHA-256 repository).
///
/// Held merges carry one: if the branch has moved, the approval no longer applies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first seven characters, as shown in the TUI (`a1b2c3d`).
    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

impl FromStr for CommitSha {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        if s.len() != 40 && s.len() != 64 {
            return Err(IdError::new(
                "commit SHA",
                s,
                "a full SHA has 40 (or 64) hex digits",
            ));
        }
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(IdError::new("commit SHA", s, "only hex digits are allowed"));
        }
        Ok(Self(s.to_ascii_lowercase()))
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A model as its account names it, e.g. `claude-opus-5-5`. Never empty, never containing
/// whitespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Result<Self, IdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(IdError::new("model ID", &id, "it is empty"));
        }
        if id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(IdError::new("model ID", &id, "it contains whitespace"));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ModelId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An engine's own reference to a session, stored on the agent so the session can be resumed:
/// `claude`'s session ID, or the provider engine's session file name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionRef(String);

impl SessionRef {
    pub fn new(reference: impl Into<String>) -> Result<Self, IdError> {
        let reference = reference.into();
        if reference.trim().is_empty() {
            return Err(IdError::new("session reference", &reference, "it is empty"));
        }
        Ok(Self(reference))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SessionRef {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl fmt::Display for SessionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A background job as its engine names it: `J3` on the provider engine, which numbers its jobs per
/// agent ([`JobId`]), or Claude Code's own name for the job on Claude. The engine's `JobStarted`
/// and `JobFinished` events carry it, and it is unique per agent (`jobs.label`). Never empty, never
/// containing whitespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobLabel(String);

impl JobLabel {
    pub fn new(label: impl Into<String>) -> Result<Self, IdError> {
        let label = label.into();
        if label.is_empty() {
            return Err(IdError::new("job label", &label, "it is empty"));
        }
        if label.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(IdError::new("job label", &label, "it contains whitespace"));
        }
        Ok(Self(label))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The provider engine's job number, when the label is one (`J3`).
    pub fn job_id(&self) -> Option<JobId> {
        self.0.parse().ok()
    }

    /// A crewmate's background helper agent as one of its jobs, on either engine: `helper:<id>`.
    /// Its report and settling wait until it has finished.
    pub fn helper(id: &str) -> Result<Self, IdError> {
        Self::new(format!("{HELPER_JOB_PREFIX}{id}"))
    }
}

/// The label prefix of a helper agent's job ([`JobLabel::helper`]).
pub const HELPER_JOB_PREFIX: &str = "helper:";

impl From<JobId> for JobLabel {
    fn from(id: JobId) -> Self {
        Self(id.to_string())
    }
}

impl FromStr for JobLabel {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, IdError> {
        Self::new(s)
    }
}

impl fmt::Display for JobLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Serde, schema and SQL for the string-backed IDs above.
macro_rules! string_id_impls {
    ($name:ident) => {
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = Cow::<'de, str>::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn inline_schema() -> bool {
                true
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string" })
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                v.as_str()?
                    .parse()
                    .map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
            }
        }
    };
}

string_id_impls!(CommitSha);
string_id_impls!(ModelId);
string_id_impls!(SessionRef);
string_id_impls!(JobLabel);

/// A task's block of ports: ten ports from `base`, passed as `QM_PORT_BASE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortBlock {
    pub base: u16,
}

impl PortBlock {
    /// Ports per task.
    pub const SIZE: u16 = 10;

    /// Every port in the block.
    pub fn ports(self) -> std::ops::RangeInclusive<u16> {
        self.base..=self.base.saturating_add(Self::SIZE - 1)
    }
}
