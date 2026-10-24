//! IDs.
//!
//! **Display IDs** are what agents and you see and type: `C-0042` (a crewmate: its fixed name),
//! `T-0017` (a task: a label grouping crewmates, which can change), `S-02` (a secondmate: its
//! fixed name), `R-0193` (correlation ID of a request routed to a secondmate), `J3` (background
//! job), `SI-5` (standing instruction) and `SC-7` (schedule). Each wraps the positive integer that
//! is its database key; it serialises as its display string (so tool arguments, tool results and
//! events read naturally) and is stored as an `INTEGER`. Parsing is strict about the prefix and
//! lenient about zero padding: `C-42` parses and displays as `C-0042`. An ID whose prefix changed
//! still parses in its older form (`SM-02`, `C-0193` for a correlation, `T-0042` for a crewmate),
//! so stored history keeps working. Tool arguments are different: there a `T-` names a task, and
//! the tool layer resolves it to that task's crewmate before anything reads it as a crewmate.
//!
//! A crewmate's own folder derives from its name (`tasks/C-0042`); its artifacts and scratchpad
//! from its task and name (`artifacts/T-0017/C-0042`). Its branch is named after its work and
//! recorded with it (`fix/upload-retry-on-slow-tmp`).
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
        display_id!($(#[$meta])* $name, $kind, $prefix, $width, $pattern, legacy []);
    };
    ($(#[$meta:meta])* $name:ident, $kind:literal, $prefix:literal, $width:literal, $pattern:literal, legacy [$($legacy:literal),*]) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroU32);

        impl $name {
            /// The fixed prefix of the display form.
            pub const PREFIX: &'static str = $prefix;

            /// Older prefixes still accepted when parsing; the display form never uses them.
            pub const LEGACY_PREFIXES: &'static [&'static str] = &[$($legacy),*];

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
                let digits = std::iter::once(Self::PREFIX)
                    .chain(Self::LEGACY_PREFIXES.iter().copied())
                    .find_map(|prefix| s.strip_prefix(prefix))
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
    /// A crewmate, shown as `C-0042`: its fixed name, which never changes. Its work is a row of
    /// the store's `tasks` table, keyed by this number. `T-0042`, the older form, still parses as
    /// the same crewmate: every task was one crewmate's work before tasks became labels.
    CrewmateId, "crewmate ID", "C-", 4, "^[CT]-[0-9]+$", legacy ["T-"]
);

/// The older name of [`CrewmateId`], from when each task was exactly one crewmate's work.
pub type TaskId = CrewmateId;

display_id!(
    /// A task, shown as `T-0017`: the label that groups crewmates working towards one thing. A
    /// crewmate's task can change; a task holds any number of crewmates.
    TaskLabelId, "task ID", "T-", 4, "^T-[0-9]+$"
);
display_id!(
    /// A secondmate, shown as `S-02`: its fixed name. `SM-02`, the older form, still parses.
    SecondmateId, "secondmate ID", "S-", 2, "^SM?-[0-9]+$", legacy ["SM-"]
);
display_id!(
    /// A request routed to a secondmate, shown as `R-0193`; its answer on the parent channel
    /// carries the same ID. `C-0193`, the older form, still parses.
    CorrelationId, "correlation ID", "R-", 4, "^[RC]-[0-9]+$", legacy ["C-"]
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

impl TaskLabelId {
    /// The crewmate numbered like this task (`T-0042` → `C-0042`): whose files an older version's
    /// `artifacts/T-0042/` and `scratchpad/T-0042/` were, before tasks held crewmates.
    pub const fn legacy_crewmate(self) -> CrewmateId {
        CrewmateId(self.0)
    }
}

impl CrewmateId {
    /// The name older versions gave this crewmate's folders: `T-0042`.
    pub fn legacy_name(self) -> String {
        format!("T-{:04}", self.0)
    }

    /// The task numbered like this crewmate (`C-0042` → `T-0042`): the task a crewmate made before
    /// tasks became labels belongs to, and the one a new crewmate's own task takes when free.
    pub const fn own_label(self) -> TaskLabelId {
        TaskLabelId(self.0)
    }

    /// Prefix of the branches tasks were given before branches were named after their work.
    pub const LEGACY_BRANCH_PREFIX: &'static str = "qm/task-";

    /// The branch a task of this number had before branches were named after their work,
    /// `qm/task-0042`. Only for recognising old branches: a task's branch is its recorded one.
    pub fn legacy_branch(self) -> String {
        format!("{}{:04}", Self::LEGACY_BRANCH_PREFIX, self.0)
    }

    /// The task an old-style branch (`qm/task-0042`) names, if it is one.
    pub fn from_legacy_branch(branch: &str) -> Result<Self, IdError> {
        let digits = branch
            .strip_prefix(Self::LEGACY_BRANCH_PREFIX)
            .ok_or_else(|| IdError::new("task branch", branch, "it must start with qm/task-"))?;
        parse_number("task branch", branch, digits).map(Self)
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
row_id!(
    /// A row in `release_launches`: one proposed release tag.
    ReleaseLaunchId
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

/// The numbers a fresh ID is drawn from. Display IDs are drawn from these ranges (zero-padded in
/// their display form); row IDs are database keys with no display form.
pub mod space {
    use std::ops::RangeInclusive;

    /// Crewmates: `C-` 1–9999 (the store's `tasks` rows).
    pub const TASK: RangeInclusive<u32> = 1..=9_999;
    /// Task labels: `T-` 1–9999.
    pub const TASK_LABEL: RangeInclusive<u32> = 1..=9_999;
    /// Secondmates: `S-` 1–99.
    pub const SECONDMATE: RangeInclusive<u32> = 1..=99;
    /// Requests routed to a secondmate: `R-` 1–9999.
    pub const CORRELATION: RangeInclusive<u32> = 1..=9_999;
    /// Background jobs: `J` 1–999.
    pub const JOB: RangeInclusive<u32> = 1..=999;
    /// Standing instructions: `SI-` 1–999.
    pub const STANDING_INSTRUCTION: RangeInclusive<u32> = 1..=999;
    /// Schedules: `SC-` 1–999.
    pub const SCHEDULE: RangeInclusive<u32> = 1..=999;
    /// Turns and messages: up to 999,999.
    pub const TURN: RangeInclusive<u32> = 1..=999_999;
    /// Turns and messages: up to 999,999.
    pub const MESSAGE: RangeInclusive<u32> = 1..=999_999;
    /// Every other row: up to 99,999,999.
    pub const ROW: RangeInclusive<u32> = 1..=99_999_999;
}

/// Where a fresh number comes from. The app draws at random ([`RandomIds`]); tests script them.
pub trait IdSource: Send {
    /// A number from `min` to `max`, inclusive. It ignores what is in use: the caller checks the
    /// number and asks again when it is taken.
    fn number(&mut self, min: u32, max: u32) -> u32;
}

/// The app's source: uniform random numbers from the operating system's generator.
#[derive(Debug)]
pub struct RandomIds(rand::rngs::StdRng);

impl RandomIds {
    /// A source seeded from the operating system.
    pub fn new() -> Self {
        use rand::SeedableRng;
        Self(rand::rngs::StdRng::from_os_rng())
    }
}

impl Default for RandomIds {
    fn default() -> Self {
        Self::new()
    }
}

impl IdSource for RandomIds {
    fn number(&mut self, min: u32, max: u32) -> u32 {
        use rand::Rng;
        self.0.random_range(min..=max)
    }
}

/// Draws this many times before listing the free numbers.
const FREE_DRAWS: u32 = 32;

/// A number from `space` that isn't in `taken`: a few random draws, then one picked at random
/// from the free numbers. `None` when every number in `space` is taken.
pub fn free_number(
    source: &mut dyn IdSource,
    space: &std::ops::RangeInclusive<u32>,
    taken: &std::collections::BTreeSet<u32>,
) -> Option<u32> {
    for _ in 0..FREE_DRAWS {
        let n = source.number(*space.start(), *space.end());
        if !taken.contains(&n) {
            return Some(n);
        }
    }
    let free: Vec<u32> = space.clone().filter(|n| !taken.contains(n)).collect();
    let last = u32::try_from(free.len().checked_sub(1)?).ok()?;
    free.get(usize::try_from(source.number(0, last)).ok()?)
        .copied()
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
