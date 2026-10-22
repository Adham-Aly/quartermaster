Report a round of your plan. The app checks $QM_PLAN_DIR/plan.md as plan_check does; on success it shows the user the page's card and you wait.
- outcome: done when the plan is ready for the user (with questions open, or final with none); blocked when something only your supervisor can fix stops you (wrong project, missing access); say what in blockers.
- answer: a short summary of the round for your supervisor: what changed and what is open. Never the plan itself; the plan is plan.md.
- blockers: what stops you, for blocked.
The user's answers come back as a message holding a <plan-reply> block. Apply them to plan.md in place, ask the follow-ups they raise, and report again. Questions for the user go only in plan.md as question blocks; there is no other way to ask them.
