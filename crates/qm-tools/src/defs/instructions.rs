//! Your standing instructions and scheduled wakes.

use super::Ack;
use jiff::Timestamp;
use qm_types::ids::{ScheduleId, StandingInstructionId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Keeps a standing instruction of yours; only on your turns.
    SaveInstructionTool {
        name: "save_instruction",
        class: Act,
        roles: [Quartermaster],
        args: SaveInstructionArgs,
        output: InstructionSaved,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveInstructionArgs {
    pub text: String,
}

/// A standing instruction was kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSaved {
    pub instruction: StandingInstructionId,
}

tool! {
    /// Removes a standing instruction of yours; only on your turns.
    DropInstructionTool {
        name: "drop_instruction",
        class: Act,
        roles: [Quartermaster],
        args: DropInstructionArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DropInstructionArgs {
    pub instruction: StandingInstructionId,
}

tool! {
    /// Schedules a wake; the time must be in the future, and is stored in UTC with your time zone.
    ScheduleTool {
        name: "schedule",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: ScheduleArgs,
        output: Scheduled,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleArgs {
    // RFC 3339 with an offset.
    pub due: Timestamp,
    pub what: String,
}

/// A wake was scheduled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scheduled {
    pub schedule: ScheduleId,
    pub due: Timestamp,
}

tool! {
    /// Cancels a scheduled wake.
    CancelScheduleTool {
        name: "cancel_schedule",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: CancelScheduleArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelScheduleArgs {
    pub schedule: ScheduleId,
}
