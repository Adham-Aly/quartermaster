Record that the user approved a plan task's plan. Only on a turn the user's own message started, and only when they said so for this plan ("I approve the plan"); never on your own judgement. The user can also approve with the button on the plan's card.
- task: the plan task.
- take_recommended: true when the user approved with the planner's recommended answer for every question still open; the planner settles them and reports, and its final plan counts as approved.
Refused for a plan under auto autonomy (it needs no approval), while the planner is updating its plan, or with questions still open unless take_recommended. Once approved, start the build the way the user asked, with builds_on the plan, then settle_task the plan.
