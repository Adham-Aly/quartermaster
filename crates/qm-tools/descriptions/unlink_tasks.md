Remove the dependency blocker -> blocked when it is obsolete. It doesn't stop, settle or deliver either task. Both must be your direct reports, with different IDs; the blocker may be finished or closed. Succeeds if the edge is already gone.
- blocker: the task whose blocking edge to remove.
- blocked: the task that depended on it.
A declared wait on that blocker clears; other blockers and waits stay. The blocked crewmate is told. Cancelled blockers aren't unlinked automatically. If the dependency held up settlement, retry settle_task; every other delivery and review check still applies.
