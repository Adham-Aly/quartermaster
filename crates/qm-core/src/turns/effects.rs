//! What a counted call did, for the effective-action rule.
//!
//! Facts from code only: the tool's name, its arguments and its result. The app never judges
//! whether an action fits the turn.

use qm_engine::ToolResult;
use qm_policy::action_rule::{Effect, app_acknowledgement};
use qm_tools::Tool;
use qm_tools::ToolClass;
use qm_tools::defs;
use qm_tools::registry;
use qm_types::domain::Role;
use qm_types::ids::TaskId;

/// What a successful call of a counting tool did. `None` for an error result or a read tool.
///
/// | Call | Effect |
/// |---|---|
/// | `reply`, `ask_user`, `acknowledge`, `present_hold`, `propose_project`, `start_secondmate`, a held `settle_task` | `Shown` |
/// | `start_crewmate` | `Started(result.task)` |
/// | `message_task` | `Steered(args.task)` |
/// | `stop_task`, `close_task` | `Stopped(args.task)` |
/// | `settle_task` (merged, delivered, landed, closed) | `Settled(args.task)` |
/// | `set_verification` / `link_tasks` | `TaskChanged(args.task)` / `TaskChanged(args.blocked)` |
/// | `schedule` / `cancel_schedule` | `ScheduleAdded` / `ScheduleCancelled` |
/// | anything else that counts, including tools added later | `SettingChanged` |
pub fn effect_of(tool: &str, args: &serde_json::Value, result: &ToolResult) -> Option<Effect> {
    if result.is_error {
        return None;
    }
    if registry::find(Role::Quartermaster, tool).is_some_and(|d| d.class == ToolClass::Read) {
        return None;
    }
    let output = || serde_json::from_str::<serde_json::Value>(&result.text).ok();
    let effect = match tool {
        t if t == defs::ReplyTool::NAME
            || t == defs::AskUserTool::NAME
            || t == defs::AcknowledgeTool::NAME
            || t == defs::PresentHoldTool::NAME
            || t == defs::ProposeProjectTool::NAME
            || t == defs::StartSecondmateTool::NAME =>
        {
            Some(Effect::Shown)
        }
        t if t == defs::StartCrewmateTool::NAME => output()
            .and_then(|o| task_at(&o, "task"))
            .map(Effect::Started),
        t if t == defs::MessageTaskTool::NAME => task_at(args, "task").map(Effect::Steered),
        t if t == defs::StopTaskTool::NAME || t == defs::CloseTaskTool::NAME => {
            task_at(args, "task").map(Effect::Stopped)
        }
        t if t == defs::SettleTaskTool::NAME => {
            let held = output()
                .and_then(|o| o.get("outcome").and_then(|v| v.as_str()).map(str::to_owned))
                .is_some_and(|o| o == "held");
            if held {
                Some(Effect::Shown)
            } else {
                task_at(args, "task").map(Effect::Settled)
            }
        }
        t if t == defs::SetVerificationTool::NAME => task_at(args, "task").map(Effect::TaskChanged),
        t if t == defs::LinkTasksTool::NAME => task_at(args, "blocked").map(Effect::TaskChanged),
        t if t == defs::ScheduleTool::NAME => Some(Effect::ScheduleAdded),
        t if t == defs::CancelScheduleTool::NAME => Some(Effect::ScheduleCancelled),
        _ => None,
    };
    // A counted call whose task can't be read back, or a tool with no row above, still changed
    // something: "Got it".
    Some(effect.unwrap_or(Effect::SettingChanged))
}

/// The words `turns.effective_actions` keeps for an effect: the app acknowledgement's line, or
/// `None` for a line shown to you.
pub fn effect_line(effect: &Effect) -> Option<String> {
    match effect {
        Effect::Shown => None,
        other => Some(app_acknowledgement(std::slice::from_ref(other))),
    }
}

fn task_at(v: &serde_json::Value, key: &str) -> Option<TaskId> {
    v.get(key)
        .and_then(|t| serde_json::from_value::<TaskId>(t.clone()).ok())
}
