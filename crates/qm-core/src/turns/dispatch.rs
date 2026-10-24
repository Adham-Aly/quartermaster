//! One tool call of a seat turn: the retry gate, dispatch, and what it did.

use super::effects::effect_of;
use qm_engine::ToolResult;
use qm_policy::action_rule::{Attempt, Effect, check_allowed_in};
use qm_tools::handler::{Dispatcher, ToolCx, ToolFailure};

/// What one call came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ran {
    pub result: ToolResult,
    /// What it did, when it counted.
    pub effect: Option<Effect>,
    /// A successful call of a counting tool.
    pub counted: bool,
}

/// Runs one call. In the retry of a turn that did nothing, every tool that changes work or settings
/// is refused with the policy's text and never dispatched; `Speak` and `Read` tools run as usual.
pub(crate) async fn run_tool(
    d: &Dispatcher,
    cx: &ToolCx<'_>,
    attempt: Attempt,
    name: &str,
    args: serde_json::Value,
) -> Ran {
    let role = cx.caller.role;
    if let Some(def) = d
        .defs()
        .iter()
        .find(|def| def.name == name && def.roles.contains(&role))
        && let Err(refusal) = check_allowed_in(attempt, def.class, name)
    {
        return Ran {
            result: ToolResult::error(ToolFailure::Refused(refusal).render()),
            effect: None,
            counted: false,
        };
    }
    let dispatched = d.dispatch(cx, name, args.clone()).await;
    let effect = if dispatched.counted {
        effect_of(
            name,
            dispatched.resolved.as_ref().unwrap_or(&args),
            &dispatched.result,
        )
    } else {
        None
    };
    Ran {
        result: dispatched.result,
        effect,
        counted: dispatched.counted,
    }
}
