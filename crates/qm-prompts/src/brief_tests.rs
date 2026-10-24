//! Regression tests for the planning instructions delivered to crewmates.

use super::*;

fn plan_facts() -> BriefFacts {
    BriefFacts {
        task: TaskId::new(42).expect("nonzero task ID"),
        kind: TaskKind::Plan,
        supervisor: BriefSupervisor::Quartermaster,
        project: "Example".into(),
        delivery: DeliveryMode::Folder,
        has_remote: false,
        verification: BriefVerification::None,
        workspace: BriefWorkspace::Folder {
            path: "/project".into(),
        },
        pr_target: None,
        ports: PortBlock { base: 42000 },
        goal: "Plan the change".into(),
        user_words: String::new(),
        instructions: Vec::new(),
        constraints: Vec::new(),
        tests_required: false,
        builds_on: None,
        repair_attempts: 0,
    }
}

#[test]
fn planning_brief_describes_both_authored_documents_and_revision_contract() {
    let brief = render_brief(&plan_facts());
    for required in [
        "comprehensive technical implementation",
        "handoff another agent can execute independently",
        "quartermaster-plan skill",
        "author plan.html from template.html",
        "XML-well-formed HTML article fragment",
        "Keep both documents current every round",
        "not prose generated from Markdown",
        "retains your source internally",
        "whole authored fragment, never edit generated runtime",
        "Call plan_check and fix all problems before every report",
        "short summary of the round",
        "Under auto autonomy there is no",
        "HTML template or review page; write only plan.md",
    ] {
        assert!(brief.contains(required), "missing {required:?}");
    }
}

#[test]
fn non_plan_brief_has_no_planning_authoring_instructions() {
    let mut facts = plan_facts();
    facts.kind = TaskKind::Research;
    let brief = render_brief(&facts);
    assert!(!brief.contains("## Planning"));
    assert!(!brief.contains("template.html"));
}

#[test]
fn interactive_prompt_preserves_the_review_loop_on_both_engines() {
    for engine in [PromptEngine::Claude, PromptEngine::Provider] {
        let prompt = crewmate_prompt(engine, &[], &[], Some(PlanMode::Interactive));
        for required in [
            "highly detailed, comprehensive technical implementation handoff",
            "starting from supplied template.html",
            "<pre data-question=\"\">escaped TOML</pre>",
            "never question fences in plan.md",
            "update both every round",
            "at least one meaningful question in round 1",
            "fix every problem and check again",
            "retains your authored source internally",
            "whole authored article fragment again",
            "<plan-reply>",
            "remove answered questions",
            "Resolve skipped questions explicitly",
            "asked you to take recommendations",
            "implementation waits for approval",
            "never at the expense of substance",
        ] {
            assert!(
                prompt.contains(required),
                "missing {required:?} on {engine:?}"
            );
        }
        assert!(!prompt.contains("only as question blocks in plan.md"));
    }
}

#[test]
fn internal_prompt_requires_an_independent_handoff_without_html() {
    for engine in [PromptEngine::Claude, PromptEngine::Provider] {
        let prompt = crewmate_prompt(engine, &[], &[], Some(PlanMode::Internal));
        for required in [
            "comprehensive technical implementation handoff",
            "Decisions made without the user",
            "with why",
            "no HTML template or review document",
            "do not create plan.html",
            "Call plan_check, fix every problem and check again",
            "build starts from plan.md",
            "Do not implement the plan yourself",
        ] {
            assert!(
                prompt.contains(required),
                "missing {required:?} on {engine:?}"
            );
        }
        assert!(!prompt.contains("<pre data-question="));
    }
}

#[test]
fn planning_tool_descriptions_match_authoring_contract_and_budget() {
    let interactive_check = include_str!("../../qm-tools/descriptions/plan_check.interactive.md");
    let interactive_report = include_str!("../../qm-tools/descriptions/report.plan_interactive.md");
    let internal_check = include_str!("../../qm-tools/descriptions/plan_check.internal.md");
    let internal_report = include_str!("../../qm-tools/descriptions/report.plan_internal.md");
    for description in [interactive_check, interactive_report] {
        for required in [
            "plan.md",
            "plan.html",
            "<pre data-question=\"\">escaped TOML</pre>",
            "whole authored article fragment",
            "generated runtime",
            "every round",
            "template.html",
        ] {
            assert!(description.contains(required), "missing {required:?}");
        }
        assert!(!description.contains("only in plan.md as question blocks"));
    }
    for description in [internal_check, internal_report] {
        assert!(description.contains("do not create plan.html"));
        assert!(description.contains("Decisions made without the user"));
    }
    for description in [
        interactive_check,
        interactive_report,
        internal_check,
        internal_report,
    ] {
        assert!(
            description.chars().count() <= 1900,
            "tool description exceeds budget"
        );
    }
}
