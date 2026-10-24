Start now; use schedule_crewmate to defer. App copies user words; returns task, branch, routing.
- folder or project: absolute/~ folder or listed project; neither means home; create_folder if user asked for a new one. in_place: only if user asked for checkout work.
- kind: build, fix, plan, research, lookup, teach, review, reproduce, unclear. Classify work, not desired strength.
- title: conventional-commit summary. goal: checkable done. constraints: text, optional freeze paths. tests_required: true if asked.
- builds_on: finished plan, confirmed reproduction/diagnosis, or work to review; checked. Plan-based code needs approval or completion under auto.
- instructions: one-time. later_steps: user-named model/tier/effort/premium for later kinds via builds_on; only if asked this turn.
- preference: {selection: ModelId REQUIRED, provider?: Provider, model?: ModelId, effort?: Effort}. Pick smartest actual routed-tier entry meeting ALL user constraints; optional fields are those constraints. Never invent provider/model/tier/effort combinations. App checks eligibility/effort; no match means ordinary routing with a note, not tier escape. Allowed on later/event turns; cannot contradict authorized model/tier/effort overrides.
- model, tier, effort, premium: explicit user request only. choice_message: actual user-message numeric ID for matching, unsuperseded earlier authority in original scope; omit for this turn. Use user_message_sources or find_user_requests. App checks ownership; match fields/scope, not reports/events. Premium: set premium, not its model name; task-only unless later steps requested.
- base_branch, verification, stronger_retry_of: only if user asked this turn; last retries failed work stronger.
Top-level tier selects ranked low/medium/high/premium pool; effort changes reasoning only. Medium-tier ≠ medium-effort (likewise high/low). Named model busy: queue it, never substitute.
