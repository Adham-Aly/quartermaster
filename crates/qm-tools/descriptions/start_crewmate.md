Start now; use schedule_crewmate to defer. App copies user words; returns crewmate (C-n), task (T-n), branch, routing.
- join_task: same work's open T-n; omitted: builds_on's/stronger_retry_of's if open, else new.
- folder or project: absolute/~ folder or listed project; neither means home; create_folder if user asked. in_place: only if user asked for checkout work.
- kind: build, fix, plan, research, lookup, teach, review, reproduce, unclear. Classify work, not strength.
- title: conventional commit. goal: checkable done. constraints: text, optional freeze paths. tests_required: true if asked.
- builds_on: finished plan, confirmed reproduction/diagnosis, or work to review; checked. Plan-based code: approved, or done under auto.
- instructions: one-time. later_steps: user-named model/tier/effort/premium for later kinds via builds_on; only if asked this turn.
- preference: {selection: ModelId REQUIRED, provider?: Provider, model?: ModelId, effort?: Effort}. Pick smartest actual routed-tier entry meeting ALL user constraints (the optional fields). Never invent combinations. App checks eligibility/effort; no match -> ordinary routing with note, never tier escape. Allowed on later/event turns; can't contradict authorized overrides.
- model, tier, effort, premium: explicit user request only. choice_message: actual user-message numeric ID for matching, unsuperseded earlier authority in original scope; omit for this turn. Use user_message_sources or find_user_requests. App checks ownership; match fields/scope, not reports/events. Premium: set premium, not its model name; task-only unless later steps requested.
- base_branch, verification, stronger_retry_of: only if user asked this turn; last retries failed work stronger.
Top-level tier selects ranked low/medium/high/premium pool; effort changes reasoning only. Medium-tier ≠ medium-effort (likewise high/low).
