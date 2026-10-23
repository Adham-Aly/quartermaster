Read what GitHub shows for a task's release tags, after the user approved one and the app pushed it. Read only: it never pushes, triggers a workflow or downloads anything. Each launch reports three separate facts:
- tag_status: whether the tag is published (GitHub shows it at the app's own object), or what stopped it.
- release: the GitHub release for the tag: none yet, a draft (nothing offered for download), or published with how many of its files are uploaded. A tag alone never means a release is available.
- workflows: the workflow runs the tag started and how they ended, "none set up" only when GitHub counts no workflow, all disabled, or none started yet. Runs on the same commit for a branch are counted apart.
- watch: whether the app is still watching (for up to 3 hours after publication) and when it last read GitHub; "unavailable" means GitHub refused or didn't answer that read, never that the thing is absent.
A reading older than a minute is read again. When the watch ends, the app wakes the launch's supervisor with the last reading, so there's no need to poll.
- task: the task whose crewmate proposed the tag, e.g. T-0042.
- tag: only this tag's launch (optional).
