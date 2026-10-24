Start now in scope; use schedule_crewmate to defer. App copies user words and fills base branch.
- folder or project: absolute/~ folder or listed project in scope. in_place: only if user asked for checkout work.
- correlation: request ID (C-0193); pass on every task serving it. App applies user words, branch, authorized model/effort/premium/later_steps. Don't reassert routed choices.
- base_branch: omit; if supplied must match request, standing branch or default.
- kind: build, fix, plan, research, lookup, teach, review, reproduce, unclear. Classify work, not desired strength.
- title: conventional-commit summary. goal: checkable done. constraints: text, optional freeze paths. tests_required: true if asked.
- builds_on: finished plan, reproduction/diagnosis, work to review. Plan-based code needs approval or completion under auto.
- instructions: one-time, including relevant request instructions.
- preference: {selection: ModelId REQUIRED, provider?: Provider, model?: ModelId, effort?: Effort}. Pick smartest actual routed-tier entry meeting ALL user constraints; optional fields are those constraints. Never invent provider/model/effort combinations. App checks eligibility/effort; no match means ordinary routing with a note, not tier escape. Allowed on later/event turns; cannot contradict authorized model/effort overrides, including correlation's.
- model, effort, premium: explicit user request only. choice_message: actual user-message numeric ID for still-applicable earlier choice; omit for this turn. App checks source/ownership; match fields/scope, not reports/events. Set premium, not its model name.
- later_steps, verification, stronger_retry_of: only if user asked this turn; later_steps names model/effort/premium by kind; last retries failed work stronger.
Tier is model strength, not reasoning effort. Named model busy: queue it, never substitute.
