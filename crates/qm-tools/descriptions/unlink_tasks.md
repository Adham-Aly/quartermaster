Remove only the directed dependency edge blocker -> blocked when it is obsolete; this does not stop either task or settle/deliver work. Both tasks must exist and be your direct reports, with different IDs. The blocker may be terminal or closed. Idempotent: succeeds even if the edge is already absent.
- blocker: the task whose blocking edge to remove.
- blocked: the task that depended on it.
Clears a declared task wait only when it matches the removed blocker; other blockers and task/decision waits remain. Notifies the blocked crewmate; an external wait ends on its next turn as usual. Cancelled blockers are not automatically unlinked. If an obsolete dependency prevented settlement, retry settle_task after unlinking; all other delivery and review checks still apply.
Refused if either task doesn't exist, isn't your direct report, or the IDs are the same.
