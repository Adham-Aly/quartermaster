Mark one task as blocking another, so the blocked task waits until the blocker finishes. Both tasks must exist and be your direct reports; cycles are refused.
- blocker: the task that must finish first.
- blocked: the task that waits for it.
Cancelling or closing a blocker does not automatically remove this dependency. If it becomes obsolete, explicitly remove this directed edge with unlink_tasks, then retry settle_task on the blocked task when ready.
