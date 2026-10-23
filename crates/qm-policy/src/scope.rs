//! Scope and chain of command.
//!
//! A secondmate's scope is words that guide its judgment; they refuse nothing.
//!
//! "Every tool that acts on a task checks that the caller started it, so the Quartermaster can see
//! T-0045 under SM-02 but can only talk to SM-02. Chain of command is a code rule."
//!
//! `message_task`, `stop_task` and `close_task` act on a "direct report only";
//! `route_to_secondmate`'s "target is its own secondmate".

use qm_types::domain::Role;
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, SecondmateId, TaskId};

/// Refuses acting on a task the caller didn't start: "direct report only".
pub fn check_direct_report(
    caller: AgentId,
    task: TaskId,
    started_by: AgentId,
    owner_label: &str,
) -> Result<(), Refusal> {
    if caller == started_by {
        Ok(())
    } else {
        Err(Refusal::new(format!(
            "{task} isn't your direct report; it belongs to {owner_label}, so talk to them instead"
        )))
    }
}

/// `route_to_secondmate`: the target is the caller's own secondmate.
pub fn check_own_secondmate(
    caller: AgentId,
    secondmate: SecondmateId,
    parent: AgentId,
) -> Result<(), Refusal> {
    if caller == parent {
        Ok(())
    } else {
        Err(Refusal::new(format!("{secondmate} isn't your secondmate")))
    }
}

/// What only you may do, never through an agent: autonomy and a project's verification level;
/// projects, standing instructions and further secondmates belong to the Quartermaster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservedAction {
    ChangeAutonomy,
    ChangeProjectVerification,
    RegisterOrConfigureProject,
    StandingInstructions,
    StartSecondmate,
}

/// Refuses an action reserved for you or for the Quartermaster.
pub fn check_reserved(role: Role, action: ReservedAction) -> Result<(), Refusal> {
    match action {
        ReservedAction::ChangeAutonomy => Err(Refusal::new(
            "only the user changes autonomy, from settings; no agent can",
        )),
        ReservedAction::ChangeProjectVerification => Err(Refusal::new(
            "only the user changes a project's verification level, from settings",
        )),
        _ if role == Role::Quartermaster => Ok(()),
        _ => Err(Refusal::new(
            "projects, standing instructions and secondmates belong to the Quartermaster",
        )),
    }
}
