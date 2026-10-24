Start work now; for work that must not start yet, use schedule_crewmate. The app chooses model/effort from work facts, copies the user's words into the brief, and returns the task, branch and routing.
- folder or project: a folder (absolute or from ~) or listed project; neither means home. in_place: only when the user explicitly asked to work in the git checkout, not a worktree.
- kind: build, fix, plan, research, lookup, teach, review, reproduce or unclear. Choose from the work, not model strength; unclear runs at full strength.
- title: conventional-commit summary. goal: checkable definition of done.
- constraints: text and optional freeze paths. tests_required: true if the user asked for tests.
- builds_on: finished plan, reproduction reporting "reproduced: yes", diagnosis, or task under review. Checked as fact; finished thinking may lower routing. Code on a plan needs approval, or completion under auto.
- instructions: one-time instructions. later_steps: user-named model/effort/premium for later kinds; applied when work builds on this task, only if asked on this turn.
- model, effort, premium: only as explicitly requested by the user. choice_message: actual user-message numeric ID authorizing that choice, including a still-applicable earlier message; omit for this turn's message. App checks user source and ownership; you must match the requested fields and scope, not infer permission from reports/events. Premium is task-only: set premium, not its model name; use later_steps only if asked for those steps.
- base_branch, verification, stronger_retry_of: only if asked in the user's own message on this turn. stronger_retry_of reruns failed work as a new task on a stronger model.
