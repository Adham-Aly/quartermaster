//! The one registry of tool definitions.
//!
//! Add new tools at the end; never reorder. Every tool is listed once per variant; [`for_role`]
//! picks what one agent is offered on one engine.

use crate::defs;
use crate::spec::ToolDef;
use qm_engine::ToolDescriptor;
use qm_types::domain::{EngineKind, Role};

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
    ToolDef::of::<defs::ProposeProjectTool>(),
    ToolDef::of::<defs::ConfigureProjectTool>(),
    ToolDef::of::<defs::RemoveProjectTool>(),
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
];

/// The tools one role is offered on one engine, in registry order.
pub fn for_role(role: Role, engine: EngineKind) -> impl Iterator<Item = &'static ToolDef> {
    TOOLS.iter().filter(move |d| d.serves(role, engine))
}

/// The tool a role calls by this name.
pub fn find(role: Role, name: &str) -> Option<&'static ToolDef> {
    TOOLS
        .iter()
        .find(|d| d.name == name && d.roles.contains(&role))
}

/// What goes in a session spec's `tools`: the role's tools whose calls come back to the core, the
/// same on every engine. The provider engine's file and shell tools are not among them: they run
/// inside that engine, which offers them itself when the spec asks for
/// `Builtins::ProviderFileTools`.
pub fn descriptors(role: Role) -> Vec<ToolDescriptor> {
    TOOLS
        .iter()
        .filter(|d| d.roles.contains(&role) && d.scope == crate::EngineScope::Both)
        .map(ToolDef::descriptor)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::TOOLS;

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
}
