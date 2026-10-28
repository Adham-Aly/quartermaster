//! Regression tests for authored task briefs and planning instructions delivered to crewmates.

use super::*;

fn plan_facts() -> BriefFacts {
    BriefFacts {
        task: TaskId::new(42).expect("nonzero task ID"),
        label: TaskLabelId::new(42).expect("nonzero task ID"),
        kind: TaskKind::Plan,
        supervisor: BriefSupervisor::Quartermaster,
        project: "Example".into(),
        delivery: DeliveryMode::Folder,
        has_remote: false,
        verification: BriefVerification::None,
        workspace: BriefWorkspace::Folder {
            path: "/project".into(),
            repository: false,
        },
        pr_target: None,
        ports: PortBlock { base: 42000 },
        goal: "Plan the change".into(),
        instructions: Vec::new(),
        constraints: Vec::new(),
        tests_required: false,
        builds_on: None,
        repair_attempts: 0,
    }
}

#[test]
fn every_kind_and_supervisor_renders_authored_task_without_a_source_words_section() {
    for &kind in TaskKind::ALL {
        for supervisor in [
            BriefSupervisor::Quartermaster,
            BriefSupervisor::Secondmate(SecondmateId::new(2).expect("nonzero secondmate ID")),
        ] {
            let mut facts = plan_facts();
            facts.kind = kind;
            facts.supervisor = supervisor;
            facts.goal = "Count the lines in the assigned file".into();
            facts.instructions = vec!["Report the count and how you checked it".into()];
            let brief = render_brief(&facts);
            let from = match supervisor {
                BriefSupervisor::Quartermaster => "quartermaster".to_owned(),
                BriefSupervisor::Secondmate(id) => id.to_string(),
            };
            assert!(brief.starts_with(&format!(
                "<qm:supervisor from=\"{from}\" task=\"{}\">\n",
                facts.task
            )));
            assert!(brief.ends_with("</qm:supervisor>\n"));
            assert!(brief.contains("## Goal\nCount the lines in the assigned file\n"));
            assert!(brief.contains(
                "## Instructions for this task only (from your supervisor)\n- Report the count and how you checked it"
            ));
            for forbidden in [
                "## The user's own words",
                "copied by the app",
                "these outrank",
                "Your supervisor left out parts of this message",
            ] {
                assert!(
                    !brief.contains(forbidden),
                    "unexpected {forbidden:?} for {kind} / {supervisor:?}"
                );
            }

            facts.instructions = vec!["  ".into()];
            let brief = render_brief(&facts);
            assert!(brief.contains("Count the lines in the assigned file"));
            assert!(!brief.contains("## Instructions for this task only"));
            assert!(!brief.contains("## The user's own words"));
        }
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
    for &kind in TaskKind::ALL {
        if kind == TaskKind::Plan {
            continue;
        }
        facts.kind = kind;
        let brief = render_brief(&facts);
        // Report templates are shared; plan authoring instructions are not.
        for forbidden in [
            "## Planning",
            "quartermaster-plan",
            "$QM_PLAN_DIR",
            "plan.md",
            "plan.html",
            "plan_check",
        ] {
            assert!(
                !brief.contains(forbidden),
                "unexpected {forbidden:?} in {kind} brief"
            );
        }
    }
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
