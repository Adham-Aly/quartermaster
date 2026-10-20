Change a registered project's settings when the user asks. Autonomy and the verification level can't be changed this way: only the user changes them, in Settings.
- project: the project's name.
- delivery: direct_pr, branch_only or build_in_place (optional).
- merge_method: merge, squash or rebase (optional). Kept for the record only: the app squash-merges every pull request.
- default_branch: the branch work starts from by default (optional).
- files_to_copy: files copied from the user's checkout into each new worktree, such as .env (optional).
- setup_command: a command run in each new worktree (optional); clear_setup_command: true removes it.
