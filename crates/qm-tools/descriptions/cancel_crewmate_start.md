Cancel one of your pending deferred crewmate starts; no child will be started by it.
- schedule: the numeric ID returned by schedule_crewmate or list_crewmate_starts (not an SC- reminder ID).
Only the owning orchestrator may cancel. A start that already fired is not undone: use stop_task for its child if needed. Use cancel_schedule for reminder wakes instead.
