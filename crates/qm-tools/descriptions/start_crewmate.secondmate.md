Start now in scope; use schedule_crewmate to defer. App copies user words/branch.
- folder or project: absolute/~ folder or scoped project; create_folder if user asked for a new one. in_place: only if user asked for checkout work.
- correlation: request ID; pass on every serving task. App applies authorized choices; don't reassert.
- base_branch: omit; if supplied must match request, standing branch or default.
- kind: build, fix, plan, research, lookup, teach, review, reproduce, unclear. Classify work, not strength.
- title: conventional commit. goal: checkable done. constraints: text, optional freeze paths. tests_required: true if asked.
- builds_on: finished plan, reproduction/diagnosis, work to review. Plan-based code needs approval or completion under auto.
- instructions: one-time, include request instructions.
- preference: {selection: ModelId REQUIRED, provider?: Provider, model?: ModelId, effort?: Effort}. Pick smartest actual routed-tier entry meeting ALL user constraints; optional fields constrain. Never invent combinations. App checks eligibility/effort; no match -> ordinary routing with note, never tier escape. Allowed on later/event turns; cannot contradict authorized model/tier/effort overrides, including correlation's.
- model, tier, effort, premium: explicit user request only. choice_message: actual user-message numeric ID for matching, unsuperseded earlier choice in original scope; omit for this turn. Use user_message_sources or find_user_requests. App checks ownership; match fields/scope, not reports/events. Set premium, not its model name.
- later_steps, verification, stronger_retry_of: only if user asked this turn; later_steps names model/tier/effort/premium by kind; last retries failed work stronger.
Top-level tier selects ranked low/medium/high/premium pool; effort changes reasoning only. Medium-tier ≠ medium-effort (likewise high/low). Named model busy: queue it, never substitute.
