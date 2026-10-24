Start now; use schedule_crewmate to defer. App copies user words; returns task, branch, routing.
- folder or project: absolute/~ folder or listed project; neither means home. in_place: only if user asked for checkout work.
- kind: build, fix, plan, research, lookup, teach, review, reproduce, unclear. Classify work, not desired strength.
- title: conventional-commit summary. goal: checkable done. constraints: text, optional freeze paths. tests_required: true if asked.
- builds_on: finished plan, confirmed reproduction/diagnosis, or work to review; checked. Plan-based code needs approval or completion under auto.
- instructions: one-time. later_steps: user-named model/effort/premium for later kinds via builds_on; only if asked this turn.
- preference: {selection: ModelId REQUIRED, provider?: Provider, model?: ModelId, effort?: Effort}. After classifying, pick smartest actual routed-tier entry meeting ALL user constraints; optional fields are those constraints. Never invent provider/model/effort combinations. App checks eligibility/effort; no match means ordinary routing with a note, not tier escape. Allowed on later/event turns; cannot contradict authorized model/effort overrides.
- model, effort, premium: explicit user request only. choice_message: actual user-message numeric ID for still-applicable earlier authority; omit for this turn. App checks source/ownership; match fields/scope, not reports/events. Premium: set premium, not its model name; task-only unless later steps requested.
- base_branch, verification, stronger_retry_of: only if user asked this turn; last retries failed work stronger.
Tier is model strength, not reasoning effort. Named model busy: queue it, never substitute.
