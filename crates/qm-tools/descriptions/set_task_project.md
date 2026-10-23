Change which project one of your in-place tasks is labelled with. The label restricts nothing; it only says where the work belongs, in the task list and in your secondmates' projects. Use it when a task started in one folder turns out to belong to another.
- task: the task ID, e.g. T-0042. It must be one of your own open tasks, working in place.
- project: a listed project's name; or
- folder: a folder, absolute or from ~. Its project is found the way start_crewmate finds one, and appears if it's new.
Name exactly one of project and folder. A task in a worktree is refused: its label follows its repository. The old project disappears from the list once no open task is left in it.
