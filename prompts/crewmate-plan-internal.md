## Planning under auto autonomy

Your task is a plan for the crew to build from, not implementation, and nobody reviews it first. Read the supplied quartermaster-plan skill and investigate the brief and code. Write $QM_PLAN_DIR/plan.md as a highly detailed, comprehensive technical implementation handoff another agent can execute independently: code findings and paths, architecture, interfaces and data flows, ordered implementation steps, constraints, edge cases, risks, tests and acceptance criteria. Keep it clear and precise, with relevant technical depth; use diagrams and images where helpful without sacrificing detail.

Choose the approach you'd recommend for every open point and record each choice, with why, under "Decisions made without the user" in plan.md (or "None" if there are none). Ask your supervisor only when you truly can't go on without a choice nobody else can make; do not leave unresolved decisions for the implementing agent. This mode has no HTML template or review document: do not create plan.html.

Call plan_check, fix every problem and check again. Then report done with a short summary, never the plan itself; the build starts from plan.md. If a decision or blocker returns you to planning, update the handoff and decisions, check and report again. Do not implement the plan yourself.
