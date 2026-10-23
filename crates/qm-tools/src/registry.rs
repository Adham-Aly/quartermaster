//! The one registry of tool definitions.
//!
//! Add new tools at the end; never reorder. Every tool is listed once per variant; [`for_role`]
//! picks what one agent is offered on one engine.

use crate::defs;
use crate::spec::ToolDef;
use qm_engine::ToolDescriptor;
use qm_types::domain::{EngineKind, PlanMode, Role};

/// Every tool definition.
pub static TOOLS: &[ToolDef] = &[
    // Quartermaster and secondmates: talking to you.
    ToolDef::of::<defs::ReplyTool>(),
    ToolDef::of::<defs::AskUserTool>(),
    ToolDef::of::<defs::AcknowledgeTool>(),
    // Starting and running work.
    ToolDef::of::<defs::StartCrewmateTool>(),
    ToolDef::of::<defs::SecondmateStartCrewmateTool>(),
    ToolDef::of::<defs::MessageTaskTool>(),
    ToolDef::of::<defs::StopTaskTool>(),
    ToolDef::of::<defs::SetTaskProjectTool>(),
    ToolDef::of::<defs::CloseTaskTool>(),
    ToolDef::of::<defs::SettleTaskTool>(),
    ToolDef::of::<defs::PresentHoldTool>(),
    ToolDef::of::<defs::LinkTasksTool>(),
    ToolDef::of::<defs::TaskStatusTool>(),
    ToolDef::of::<defs::FindTasksTool>(),
    ToolDef::of::<defs::SetVerificationTool>(),
    // Secondmates.
    ToolDef::of::<defs::StartSecondmateTool>(),
    ToolDef::of::<defs::RouteToSecondmateTool>(),
    ToolDef::of::<defs::ConfigureSecondmateTool>(),
    ToolDef::of::<defs::CloseSecondmateTool>(),
    ToolDef::of::<defs::ParentReportTool>(),
    // Projects.
    ToolDef::of::<defs::FindProjectFolderTool>(),
    ToolDef::of::<defs::CloneRepositoryTool>(),
    ToolDef::of::<defs::ConfigureProjectTool>(),
    // Standing instructions and schedules.
    ToolDef::of::<defs::SaveInstructionTool>(),
    ToolDef::of::<defs::DropInstructionTool>(),
    ToolDef::of::<defs::ScheduleTool>(),
    ToolDef::of::<defs::CancelScheduleTool>(),
    // Crewmates.
    ToolDef::of::<defs::ReportTool>(),
    ToolDef::of::<defs::AskDecisionTool>(),
    ToolDef::of::<defs::DeclareWaitTool>(),
    ToolDef::of::<defs::FileFollowupTool>(),
    ToolDef::of::<defs::DeliveryCheckTool>(),
    // Crewmates: GitHub and CI.
    ToolDef::of::<defs::PrOpenTool>(),
    ToolDef::of::<defs::PrStatusTool>(),
    ToolDef::of::<defs::CiResultsTool>(),
    ToolDef::of::<defs::CiDiagnosisTool>(),
    ToolDef::of::<defs::CiRerunTool>(),
    ToolDef::of::<defs::GithubReadTool>(),
    ToolDef::of::<defs::GithubWriteTool>(),
    // Crewmates on the provider engine: files and shell.
    ToolDef::of::<defs::ReadTool>(),
    ToolDef::of::<defs::WriteTool>(),
    ToolDef::of::<defs::EditTool>(),
    ToolDef::of::<defs::GrepTool>(),
    ToolDef::of::<defs::FindTool>(),
    ToolDef::of::<defs::LsTool>(),
    ToolDef::of::<defs::BashTool>(),
    ToolDef::of::<defs::JobOutputTool>(),
    ToolDef::of::<defs::JobKillTool>(),
    // Crewmates on the provider engine: helper agents.
    ToolDef::of::<defs::AgentTool>(),
    // Plans: the planner's mode variants, and the user's approval.
    ToolDef::of::<defs::PlanCheckInteractiveTool>(),
    ToolDef::of::<defs::PlanCheckInternalTool>(),
    ToolDef::of::<defs::PlanReportInteractiveTool>(),
    ToolDef::of::<defs::PlanReportInternalTool>(),
    ToolDef::of::<defs::AskDecisionPlanInternalTool>(),
    ToolDef::of::<defs::ApprovePlanTool>(),
    // Crewmates: release tags, created and pushed by the app after the user's approval.
    ToolDef::of::<defs::ProposeReleaseTagTool>(),
    // Supervisors: what GitHub shows after a release tag is published.
    ToolDef::of::<defs::ReleaseStatusTool>(),
];

/// The tools one role is offered on one engine, in registry order. `plan` is the plan mode a plan
/// task's crewmate's session was started in; `None` for every other caller.
pub fn for_role(
    role: Role,
    engine: EngineKind,
    plan: Option<PlanMode>,
) -> impl Iterator<Item = &'static ToolDef> {
    TOOLS.iter().filter(move |d| d.serves(role, engine, plan))
}

/// The tool a caller calls by this name: at most one variant of a name is offered to a caller.
pub fn find(role: Role, plan: Option<PlanMode>, name: &str) -> Option<&'static ToolDef> {
    TOOLS
        .iter()
        .find(|d| d.name == name && d.offered(role, plan))
}

/// What goes in a session spec's `tools`: the role's tools whose calls come back to the core, the
/// same on every engine. The provider engine's file and shell tools are not among them: they run
/// inside that engine, which offers them itself when the spec asks for
/// `Builtins::ProviderFileTools`.
pub fn descriptors(role: Role, plan: Option<PlanMode>) -> Vec<ToolDescriptor> {
    TOOLS
        .iter()
        .filter(|d| d.offered(role, plan) && d.scope == crate::EngineScope::Both)
        .map(ToolDef::descriptor)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{TOOLS, find};
    use qm_types::domain::{PlanMode, Role};

    #[test]
    fn history_tools_explain_forgetting_and_reused_numbers() {
        for name in ["close_secondmate", "task_status", "find_tasks"] {
            let tool = TOOLS.iter().find(|t| t.name == name).expect("tool");
            let text = tool.descriptor().description;
            assert!(text.contains("reused after restart"), "{name}");
            assert!(!text.contains("never reused"), "{name}");
            assert!(!text.contains("history is kept"), "{name}");
            assert!(text.len() <= 1_900, "{name}");
            if name == "find_tasks" {
                assert!(text.contains("not yet forgotten"));
            } else {
                assert!(text.contains("memory-only"));
                assert!(text.contains("lost after restart"));
            }
        }
    }

    /// Every key anywhere in `value`, lowercased.
    fn keys(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    out.push(k.to_lowercase());
                    keys(v, out);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| keys(v, out)),
            _ => {}
        }
    }

    /// Autonomy is the user's alone: no agent's tool (Quartermaster, secondmate or crewmate) is
    /// named for it or has an argument that could set it.
    #[test]
    fn no_tool_can_change_autonomy() {
        for tool in TOOLS {
            assert!(!tool.name.contains("autonomy"), "{}", tool.name);
            let mut found = Vec::new();
            keys(&tool.input_schema(), &mut found);
            assert!(
                !found.iter().any(|k| k.contains("autonomy")),
                "{} has an autonomy argument",
                tool.name
            );
        }
        assert!(TOOLS.iter().any(|t| t.name == "configure_project"));
    }

    const PLAN_CASES: [Option<PlanMode>; 3] =
        [None, Some(PlanMode::Interactive), Some(PlanMode::Internal)];

    /// Every registered tool, plan variants included, has a handler under its key.
    #[test]
    fn every_key_has_a_handler() {
        for tool in TOOLS.iter().filter(|t| t.scope == crate::EngineScope::Both) {
            assert!(
                crate::handlers::HANDLERS.iter().any(|b| b.key == tool.key),
                "{} has no handler",
                tool.key
            );
        }
    }

    /// A caller is offered at most one tool of each name, whatever its plan mode.
    #[test]
    fn names_are_unique_per_caller() {
        for role in [Role::Quartermaster, Role::Secondmate, Role::Crewmate] {
            for plan in PLAN_CASES {
                let names: Vec<&str> = TOOLS
                    .iter()
                    .filter(|d| d.offered(role, plan))
                    .map(|d| d.name)
                    .collect();
                let mut unique = names.clone();
                unique.sort_unstable();
                unique.dedup();
                assert_eq!(names.len(), unique.len(), "{role} {plan:?}: {names:?}");
            }
        }
    }

    /// Under normal autonomy a planner can reach the user only through its plan's page: no
    /// `ask_decision`, and no `needs_decision` outcome on its `report`.
    #[test]
    fn an_interactive_planner_cannot_ask_its_supervisor() {
        let plan = Some(PlanMode::Interactive);
        assert!(find(Role::Crewmate, plan, "ask_decision").is_none());
        let report = find(Role::Crewmate, plan, "report").expect("report");
        assert_eq!(report.key, "report.plan_interactive");
        let schema = report.input_schema().to_string();
        assert!(!schema.contains("needs_decision"), "{schema}");
        assert!(find(Role::Crewmate, plan, "plan_check").is_some());
    }

    /// Under auto a planner has its own `ask_decision` and `report`; other crewmates have neither
    /// `plan_check` nor a plan variant.
    #[test]
    fn variants_follow_the_plan_mode() {
        let plan = Some(PlanMode::Internal);
        assert_eq!(
            find(Role::Crewmate, plan, "ask_decision").map(|d| d.key),
            Some("ask_decision.plan_internal")
        );
        assert_eq!(
            find(Role::Crewmate, plan, "report").map(|d| d.key),
            Some("report.plan_internal")
        );
        assert!(find(Role::Crewmate, None, "plan_check").is_none());
        assert_eq!(
            find(Role::Crewmate, None, "report").map(|d| d.key),
            Some("report")
        );
        assert!(find(Role::Crewmate, None, "approve_plan").is_none());
        assert!(find(Role::Quartermaster, None, "approve_plan").is_some());
        assert!(find(Role::Secondmate, None, "approve_plan").is_some());
    }
}
